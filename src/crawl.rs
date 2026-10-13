//! Rastreo de sitio: recorre páginas HTML del mismo host y reúne enlaces a archivos que
//! cumplan los filtros. Respeta `robots.txt`, espacia las peticiones y limita el tamaño de cada página.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Duration;

use hyper::Uri;
use robotstxt::DefaultMatcher;
use scraper::{Html, Selector};
use url::Url;

use crate::error::{Error, Result};
use crate::net::{self, HttpClient};

/// Extensiones de archivo que se consideran descargables por defecto.
pub const DEFAULT_EXTENSIONS: &[&str] = &[
    "zip", "7z", "rar", "tar", "gz", "exe", "msi", "iso", "pdf", "docx", "xlsx", "pptx", "mp4",
    "mkv", "webm", "mp3", "flac",
];
/// Extensiones que se tratan como páginas a seguir.
const PAGE_EXTENSIONS: &[&str] = &["html", "htm", "php", "asp", "aspx", "jsp"];
const MAX_PAGE_BYTES: usize = 5 * 1024 * 1024;
const USER_AGENT: &str = "w11dl";

#[derive(Debug, Clone)]
pub struct CrawlOptions {
    pub start: String,
    pub extensions: Vec<String>,
    pub max_depth: u32,
    pub max_pages: usize,
    /// Pausa entre páginas, para no saturar el servidor.
    pub delay: Duration,
}

impl CrawlOptions {
    pub fn new(start: impl Into<String>) -> Self {
        Self {
            start: start.into(),
            extensions: DEFAULT_EXTENSIONS.iter().map(|s| (*s).to_owned()).collect(),
            max_depth: 2,
            max_pages: 200,
            delay: Duration::from_millis(500),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Crawl {
    /// Archivos encontrados, sin duplicados y en orden de descubrimiento.
    pub files: Vec<Url>,
    pub pages_visited: usize,
}

/// Política de `robots.txt` de un origen.
enum Robots {
    /// No hay `robots.txt`: todo permitido.
    AllowAll,
    Rules(String),
    /// No se pudo leer: por prudencia no se rastrea ese origen.
    Deny,
}

pub async fn crawl(client: &HttpClient, opts: &CrawlOptions) -> Result<Crawl> {
    let start = Url::parse(&opts.start).map_err(|_| Error::InvalidUrl(opts.start.clone()))?;
    if !matches!(start.scheme(), "http" | "https") {
        return Err(Error::UnsupportedScheme(start.scheme().to_owned()));
    }

    let mut queue = VecDeque::from([(start.clone(), 0u32)]);
    let mut seen_pages: HashSet<String> = HashSet::from([start.to_string()]);
    let mut seen_files: HashSet<String> = HashSet::new();
    let mut files = Vec::new();
    let mut robots: HashMap<String, Robots> = HashMap::new();
    let mut visited = 0usize;

    while let Some((page, depth)) = queue.pop_front() {
        if visited >= opts.max_pages {
            break;
        }
        if !allowed(client, &mut robots, &page).await? {
            continue;
        }
        if visited > 0 && !opts.delay.is_zero() {
            tokio::time::sleep(opts.delay).await;
        }
        visited += 1;

        let uri = to_uri(&page)?;
        let Some(body) = net::get_small(client, &uri, MAX_PAGE_BYTES).await? else {
            continue;
        };
        if !body.content_type.as_deref().is_some_and(is_html) {
            continue;
        }

        let html = String::from_utf8_lossy(&body.bytes);
        for link in extract_links(&page, &html) {
            if link.host_str() != start.host_str() {
                continue;
            }
            match extension(&link) {
                Some(ext) if PAGE_EXTENSIONS.contains(&ext.as_str()) => {
                    enqueue_page(&mut queue, &mut seen_pages, link, depth + 1, opts.max_depth);
                }
                Some(ext) if opts.extensions.iter().any(|e| e.eq_ignore_ascii_case(&ext)) => {
                    if allowed(client, &mut robots, &link).await?
                        && seen_files.insert(link.to_string())
                    {
                        files.push(link);
                    }
                }
                Some(_) => {}
                None => enqueue_page(&mut queue, &mut seen_pages, link, depth + 1, opts.max_depth),
            }
        }
    }

    Ok(Crawl {
        files,
        pages_visited: visited,
    })
}

fn enqueue_page(
    queue: &mut VecDeque<(Url, u32)>,
    seen: &mut HashSet<String>,
    link: Url,
    depth: u32,
    max_depth: u32,
) {
    if depth <= max_depth && seen.insert(link.to_string()) {
        queue.push_back((link, depth));
    }
}

/// Extrae enlaces `<a href>` resueltos contra la página, sin fragmentos ni esquemas no web.
pub fn extract_links(base: &Url, html: &str) -> Vec<Url> {
    let doc = Html::parse_document(html);
    let selector = Selector::parse("a[href]").expect("selector constante válido");
    doc.select(&selector)
        .filter_map(|a| a.value().attr("href"))
        .filter_map(|href| base.join(href).ok())
        .filter(|u| matches!(u.scheme(), "http" | "https"))
        .map(|mut u| {
            u.set_fragment(None);
            u
        })
        .collect()
}

async fn allowed(
    client: &HttpClient,
    robots: &mut HashMap<String, Robots>,
    url: &Url,
) -> Result<bool> {
    let origin = url.origin().ascii_serialization();
    if !robots.contains_key(&origin) {
        let rules = fetch_robots(client, &origin).await;
        robots.insert(origin.clone(), rules);
    }
    Ok(match &robots[&origin] {
        Robots::AllowAll => true,
        Robots::Deny => false,
        Robots::Rules(text) => {
            DefaultMatcher::default().one_agent_allowed_by_robots(text, USER_AGENT, url.as_str())
        }
    })
}

async fn fetch_robots(client: &HttpClient, origin: &str) -> Robots {
    let Ok(uri) = format!("{origin}/robots.txt").parse::<Uri>() else {
        return Robots::Deny;
    };
    match net::get_small(client, &uri, MAX_PAGE_BYTES).await {
        Ok(None) => Robots::AllowAll,
        Ok(Some(body)) => Robots::Rules(String::from_utf8_lossy(&body.bytes).into_owned()),
        Err(_) => Robots::Deny,
    }
}

fn to_uri(url: &Url) -> Result<Uri> {
    net::parse_url(url.as_str())
}

fn is_html(content_type: &str) -> bool {
    let media = content_type.split(';').next().unwrap_or_default().trim();
    media.eq_ignore_ascii_case("text/html") || media.eq_ignore_ascii_case("application/xhtml+xml")
}

/// Extensión en minúsculas del último segmento de la ruta, si existe.
fn extension(url: &Url) -> Option<String> {
    let segment = url.path_segments()?.next_back()?;
    let (_, ext) = segment.rsplit_once('.')?;
    if ext.is_empty() {
        None
    } else {
        Some(ext.to_ascii_lowercase())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_normaliza_a_minusculas() {
        let u = Url::parse("https://x.test/a/Archivo.ZIP?x=1").unwrap();
        assert_eq!(extension(&u).as_deref(), Some("zip"));
        assert_eq!(extension(&Url::parse("https://x.test/a/").unwrap()), None);
    }

    #[test]
    fn enlaces_se_resuelven_y_pierden_fragmento() {
        let base = Url::parse("https://x.test/dir/index.html").unwrap();
        let html = r##"<a href="a.zip#top">A</a><a href="/b.pdf">B</a><a href="mailto:z@x">M</a>"##;
        let links: Vec<String> = extract_links(&base, html)
            .iter()
            .map(Url::to_string)
            .collect();
        assert_eq!(
            links,
            vec!["https://x.test/dir/a.zip", "https://x.test/b.pdf"]
        );
    }

    #[test]
    fn content_type_html_con_parametros() {
        assert!(is_html("text/html; charset=utf-8"));
        assert!(!is_html("application/zip"));
    }
}
