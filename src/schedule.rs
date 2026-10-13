//! Descargas programadas sin proceso residente: el Programador de tareas de Windows lanza
//! `w11dl-host run-queue` a la hora indicada (despertando el equipo si hace falta), el host
//! descarga lo que esté en cola y, si todo terminó bien, ejecuta la acción final elegida.
//!
//! Aquí solo hay lógica pura (validación, XML de la tarea, órdenes de energía): lo que toca el
//! sistema se ejecuta en el binario del host y solo en Windows.

use std::path::Path;

/// Qué hacer con el equipo cuando la cola termina sin errores.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum After {
    Nothing,
    Sleep,
    Hibernate,
    Shutdown,
}

impl After {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "nothing" | "none" => Some(Self::Nothing),
            "sleep" => Some(Self::Sleep),
            "hibernate" => Some(Self::Hibernate),
            "shutdown" => Some(Self::Shutdown),
            _ => None,
        }
    }

    pub fn as_arg(self) -> &'static str {
        match self {
            Self::Nothing => "nothing",
            Self::Sleep => "sleep",
            Self::Hibernate => "hibernate",
            Self::Shutdown => "shutdown",
        }
    }

    /// Programa y argumentos que ejecutan la acción. El apagado espera 60 s y se puede abortar con
    /// `shutdown /a`, para que un error de programación no cierre el equipo sin aviso.
    pub fn command(self) -> Option<(&'static str, Vec<&'static str>)> {
        match self {
            Self::Nothing => None,
            Self::Shutdown => Some((
                "shutdown",
                vec![
                    "/s",
                    "/t",
                    "60",
                    "/c",
                    "W11DownloaderPro: descargas completadas. Para cancelar: shutdown /a",
                ],
            )),
            Self::Hibernate => Some(("shutdown", vec!["/h"])),
            // Con la hibernación activada, Windows puede hibernar en lugar de suspender.
            Self::Sleep => Some((
                "rundll32.exe",
                vec!["powrprof.dll,SetSuspendState", "0,1,0"],
            )),
        }
    }
}

/// Resultado de una ejecución de la cola.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Summary {
    pub done: usize,
    pub failed: usize,
    pub canceled: usize,
}

/// La acción final solo se ejecuta si se descargó algo y no hubo ningún fallo ni cancelación:
/// apagar con una descarga fallida dejaría al usuario sin enterarse.
pub fn should_run_after(summary: &Summary) -> bool {
    summary.done > 0 && summary.failed == 0 && summary.canceled == 0
}

/// Valida `AAAA-MM-DDTHH:MM` (hora local) y la devuelve normalizada con segundos.
pub fn parse_when(text: &str) -> Option<String> {
    let b = text.as_bytes();
    if b.len() != 16 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' {
        return None;
    }
    let num = |a: usize, z: usize| text.get(a..z)?.parse::<u32>().ok();
    let (year, month, day, hour, minute) = (
        num(0, 4)?,
        num(5, 7)?,
        num(8, 10)?,
        num(11, 13)?,
        num(14, 16)?,
    );
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => 28 + u32::from(leap),
        _ => return None,
    };
    if year < 2000 || day == 0 || day > days || hour > 23 || minute > 59 {
        return None;
    }
    Some(format!("{text}:00"))
}

/// Nombre de la tarea en el Programador: una por carpeta de descargas no hace falta; una global.
pub const TASK_NAME: &str = "W11DownloaderPro-cola";

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Definición de la tarea en XML. Se usa XML (y no argumentos de `schtasks`) porque la fecha de
/// `/SD` depende de la configuración regional del equipo y la de `StartBoundary` es ISO 8601.
/// Despierta el equipo, se ejecuta con el usuario conectado y recupera una ejecución perdida.
pub fn task_xml(when: &str, host_exe: &Path, out_dir: &Path, after: After) -> String {
    let arguments = format!(
        "run-queue --out-dir \"{}\" --after {}",
        out_dir.display(),
        after.as_arg()
    );
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>Descargas programadas de W11DownloaderPro</Description>
  </RegistrationInfo>
  <Triggers>
    <TimeTrigger>
      <StartBoundary>{when}</StartBoundary>
      <Enabled>true</Enabled>
    </TimeTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>true</RunOnlyIfNetworkAvailable>
    <WakeToRun>true</WakeToRun>
    <RestartOnFailure>
      <Interval>PT10M</Interval>
      <Count>12</Count>
    </RestartOnFailure>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Enabled>true</Enabled>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{command}</Command>
      <Arguments>{arguments}</Arguments>
    </Exec>
  </Actions>
</Task>
"#,
        when = when,
        command = xml_escape(&host_exe.display().to_string()),
        arguments = xml_escape(&arguments),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fecha_valida_se_normaliza_e_invalida_se_rechaza() {
        assert_eq!(
            parse_when("2026-10-13T03:00").as_deref(),
            Some("2026-10-13T03:00:00")
        );
        assert_eq!(
            parse_when("2028-02-29T23:59").as_deref(),
            Some("2028-02-29T23:59:00")
        );
        for bad in [
            "2027-02-29T03:00", // no bisiesto
            "2026-13-01T03:00",
            "2026-00-10T03:00",
            "2026-04-31T03:00",
            "2026-10-13T24:00",
            "2026-10-13T03:60",
            "2026-10-13 03:00",
            "13/10/2026 03:00",
            "2026-10-13T03:00:00",
            "",
        ] {
            assert_eq!(parse_when(bad), None, "{bad}");
        }
    }

    #[test]
    fn acciones_y_sus_argumentos() {
        assert_eq!(After::parse("shutdown"), Some(After::Shutdown));
        assert_eq!(After::parse("none"), Some(After::Nothing));
        assert_eq!(After::parse("reboot"), None);
        assert!(After::Nothing.command().is_none());
        let (prog, args) = After::Shutdown.command().unwrap();
        assert_eq!(prog, "shutdown");
        assert!(
            args.windows(2).any(|w| w == ["/t", "60"]),
            "debe dar 60 s para abortar"
        );
        assert_eq!(After::Hibernate.command().unwrap().1, vec!["/h"]);
        for a in [
            After::Nothing,
            After::Sleep,
            After::Hibernate,
            After::Shutdown,
        ] {
            assert_eq!(After::parse(a.as_arg()), Some(a));
        }
    }

    #[test]
    fn solo_actua_si_todo_salio_bien() {
        let ok = Summary {
            done: 3,
            failed: 0,
            canceled: 0,
        };
        assert!(should_run_after(&ok));
        assert!(!should_run_after(&Summary { failed: 1, ..ok }));
        assert!(!should_run_after(&Summary { canceled: 1, ..ok }));
        assert!(
            !should_run_after(&Summary::default()),
            "sin descargas no se apaga nada"
        );
    }

    #[test]
    fn xml_de_la_tarea_escapa_rutas_y_fija_la_hora() {
        let xml = task_xml(
            "2026-10-13T03:00:00",
            Path::new(r"C:\Users\Ana & Co\w11dl-host.exe"),
            Path::new(r"C:\Descargas <x>"),
            After::Shutdown,
        );
        assert!(xml.contains("<StartBoundary>2026-10-13T03:00:00</StartBoundary>"));
        assert!(xml.contains("Ana &amp; Co"));
        assert!(xml.contains("&lt;x&gt;"));
        assert!(xml.contains("--after shutdown"));
        assert!(xml.contains("<WakeToRun>true</WakeToRun>"));
        assert!(xml.contains("<StartWhenAvailable>true</StartWhenAvailable>"));
        // Si la cola está en uso por el host del navegador, `run-queue` sale con error y se reintenta.
        assert!(xml.contains("<RestartOnFailure>") && xml.contains("<Count>12</Count>"));
        assert!(!xml.contains("Ana & Co"), "sin escapar rompería el XML");
    }
}
