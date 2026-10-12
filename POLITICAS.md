# Políticas de desarrollo — W11DownloaderPro

Este documento es vinculante para todo código, commit y decisión técnica del proyecto.

## 1. Tecnología

1. Se usa la versión **estable más reciente** de cada lenguaje, runtime, framework y dependencia. Antes de fijar una versión se verifica en la fuente oficial (sitio del proveedor o registro del paquete). No se adoptan versiones preliminares (alpha, beta, RC) en código de producción.
2. Cada dependencia debe justificarse. Si la biblioteca estándar del lenguaje resuelve el problema con calidad equivalente, se usa la biblioteca estándar.
3. Las versiones quedan fijadas mediante archivo de bloqueo (lockfile) commiteado.
4. Una dependencia sin mantenimiento activo, con vulnerabilidades abiertas o con licencia incompatible no se incorpora.

## 2. Eficiencia: correcta antes que corta

1. **Primero corrección, después rendimiento.** Ninguna optimización justifica código incorrecto o ilegible.
2. **Medir antes de optimizar.** Toda optimización de rendimiento incluye una medición antes y después (benchmark o perfil). Sin medición, no se acepta el cambio.
3. **Elegir el algoritmo y la estructura de datos adecuados** antes que micro-optimizar. Complejidad y uso de memoria se documentan cuando son relevantes.
4. **Concurrencia y E/S:** las descargas usan E/S asíncrona o multihilo acotado, reutilizan conexiones (keep-alive) y limitan el número de conexiones simultáneas. Ningún recurso (socket, archivo, buffer) se deja abierto: la liberación es determinista.
5. **Sin trabajo innecesario:** no se recalcula lo que puede almacenarse de forma segura, ni se copian datos grandes cuando basta una referencia o un flujo.
6. **Código simple.** Menos código es preferible, siempre que sea el código correcto. No se añade abstracción sin un segundo caso de uso real.

## 3. Calidad mínima para integrar

Un cambio solo se integra si:

- Compila o se ejecuta sin errores ni advertencias en la versión de referencia.
- Pasa el linter y el formateador configurados.
- Tiene pruebas para la lógica nueva o modificada, incluido el caso de error.
- No introduce secretos, credenciales ni rutas personales en el código.
- Los errores se manejan explícitamente: no se ignoran excepciones ni códigos de retorno.

## 4. Autoría y firma

1. **Solo el nombre del dueño del repositorio** (ReinierTutoriales) aparece como autor o titular en el código, la documentación, los encabezados de archivo, la licencia y los metadatos.
2. Los commits se realizan con la identidad del dueño. No se añaden líneas `Co-Authored-By` ni firmas de terceros o de herramientas.
3. No se usan nombres de otras personas, organizaciones o herramientas como firma de autoría.
4. Ningún texto publicado en el repositorio (commits, descripciones de PR, comentarios, revisiones, código o documentación) lleva el nombre de un asistente o de una herramienta, ni líneas de firma automáticas. Si una plataforma añade una firma, se elimina. La autoría es siempre del dueño.

## 5. Flujo de trabajo: proponer antes de ejecutar

1. Todo cambio no trivial se describe **antes** de implementarse: qué se va a hacer, por qué, qué archivos se tocan y qué alternativas se descartaron.
2. El cambio se implementa solo después de la aprobación del dueño.
3. Los cambios triviales (correcciones tipográficas, formato) no requieren propuesta previa.
4. Las acciones irreversibles o visibles para terceros (publicar, subir a un repositorio externo, eliminar datos) requieren confirmación explícita en cada caso.

## 6. Estilo de código

1. Nombres descriptivos y consistentes con el lenguaje elegido.
2. Los comentarios explican el **porqué**, no el qué.
3. Una función hace una cosa. Las funciones largas y con muchos efectos laterales se dividen.
4. El código sigue el estilo del entorno del lenguaje (formateador y convenciones oficiales), no uno propio.

## 7. Seguridad y fiabilidad de descargas

1. Validar siempre las URLs, los encabezados de respuesta y los tamaños antes de escribir en disco.
2. Escribir a archivos temporales y renombrar al completar, para no dejar archivos corruptos.
3. Reanudar descargas solo si el servidor confirma el soporte de rangos y la validación del contenido.
4. Respetar los límites de tasa de los servidores y no evadir mecanismos de protección.
5. No almacenar credenciales en texto plano.

## 8. Revisión y vigencia

- Este documento se revisa cuando cambie la versión de referencia de la tecnología o cuando el dueño lo indique.
- Ante conflicto entre una regla de este documento y una petición puntual, prevalece el documento salvo que el dueño apruebe la excepción por escrito.
