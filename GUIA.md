# Proyecto: "VTDrop" — widget flotante minimalista para escanear archivos con VirusTotal

## Rol
Eres un desarrollador senior de aplicaciones nativas para Windows. Escribe código de producción, limpio y comentado.

## Stack (obligatorio)
- Rust estable, crate `windows` (windows-rs) para Win32, Direct2D/DirectWrite y DirectComposition.
- HTTP: `ureq` o `reqwest` (blocking, rustls). Hash: `sha2`. JSON: `serde`/`serde_json`.
- Sin frameworks de GUI, sin WebView, sin Electron.
- Objetivo: .exe único y portable < 3 MB (release con `opt-level="z"`, `lto=true`, `strip=true`, `panic="abort"`), RAM en reposo < 15 MB, 0 % de CPU en reposo (nada de bucles ni timers activos cuando no hay animación).
- Target: Windows 11 (x64). Debe degradar con elegancia en Windows 10.

## Interfaz
- Una ventana sin bordes, sin barra de tareas (WS_EX_TOOLWINDOW), siempre encima (opcional), de unos 64×64 px.
- Solo muestra un icono de escudo. Respeta el modo claro/oscuro del sistema y el color de acento.
- Se puede arrastrar con el mouse para moverla. Guarda la posición y la restaura al iniciar.
- Compatible con escalado DPI (Per-Monitor V2).
- Menú contextual (clic derecho), con el estilo nativo de Windows 11:
  Siempre encima ✓ · Iniciar con Windows ✓ · Configurar API key… · Abrir último resultado · Salir.

### Fondo translúcido estilo Windows 11
- Preferir un acrylic que SIGA ACTIVO aunque la ventana no tenga foco (es un widget,
  casi siempre está inactivo). Usar SetWindowCompositionAttribute con
  ACCENT_ENABLE_ACRYLICBLURBEHIND + GradientColor con tinte según el tema claro/oscuro.
- Alternativa opcional en la configuración: el backdrop oficial DWMWA_SYSTEMBACKDROP_TYPE =
  DWMSBT_TRANSIENTWINDOW (Win11 22621+), aceptando que se vuelve sólido sin foco.
  No usar Mica (DWMSBT_MAINWINDOW): en una ventana tan pequeña se ve como un color plano.
- NO usar WS_EX_LAYERED/UpdateLayeredWindow: rompe el backdrop. Usar
  DwmExtendFrameIntoClientArea con márgenes -1 y limpiar el fondo con alpha 0 en Direct2D.
- Esquinas: DWMWA_WINDOW_CORNER_PREFERENCE = DWMWCP_ROUND. Borde sutil de 1 px
  (DWMWA_BORDER_COLOR) que toma el color de acento al hacer hover.
- Fallback (Win10 o transparencias desactivadas en Configuración): color sólido
  del tema. Detectarlo con la clave de registro EnableTransparency
  (HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize) y reaccionar
  a WM_SETTINGCHANGE en caliente.
- Si el desenfoque permanente provoca parpadeos al arrastrar la ventana, cambiar automáticamente a DWMSBT_TRANSIENTWINDOW.
- Modo debug: una tecla alterna entre acrylic permanente / acrylic del sistema / sólido, para comparar.

## Animaciones (suaves, de 60 fps o más, y solo mientras ocurren)
- Al pasar un archivo por encima (DragEnter): el escudo crece un 10 % y aparece un brillo con el color de acento (ease-out, 150 ms).
- Al calcular el hash o consultar: un anillo de progreso gira alrededor del escudo.
- Resultado: el escudo se transforma en una pastilla que muestra el ratio:
  - 🟢 verde si son 0 detecciones, 🟡 ámbar si son 1–3, 🔴 rojo si son 4 o más (umbrales configurables).
  - Ejemplo: `0/72`. Animación de expansión con easing tipo spring, de 250 ms.
  - Tras 8 s (configurable) vuelve al escudo con un fade.
- Un clic sobre el resultado abre `https://www.virustotal.com/gui/file/{sha256}` en el navegador por defecto.
- Respeta la opción del sistema "Efectos de animación" (SPI_GETCLIENTAREAANIMATION): si están desactivados, usa transiciones instantáneas.

## Flujo
1. Recibe archivos con IDropTarget (OLE) o WM_DROPFILES. Acepta uno o varios archivos; ignora las carpetas o recórrelas solo si el usuario lo activa.
2. Calcula el SHA-256 en streaming (con buffer de 1 MB) en un hilo de trabajo. La UI nunca debe bloquearse.
3. `GET https://www.virustotal.com/api/v3/files/{sha256}` con el header `x-apikey`.
   - 200 → lee `data.attributes.last_analysis_stats`. Muestra (malicious + suspicious) / (malicious + suspicious + undetected + harmless).
   - 404 → muestra la pastilla "¿Subir?" con dos botones (✓ / ✕) y un aviso breve: "El archivo será visible para usuarios de VirusTotal".
     NUNCA subas un archivo sin confirmación explícita.
4. Subida (si se confirma):
   - Hasta 32 MB: `POST /api/v3/files` (multipart).
   - Más de 32 MB: primero `GET /api/v3/files/upload_url`, y luego POST a esa URL.
   - Después consulta `GET /api/v3/analyses/{id}` con backoff (15 s, 30 s, 60 s…) hasta `status == "completed"`, mostrando el anillo de progreso.
5. Con varios archivos: procesa una cola respetando el rate limit de la API pública (4 req/min, configurable). Muestra un contador "2/5" y al final un resumen con el peor resultado. Una notificación toast de Windows lista cada archivo con su ratio.
6. Errores (401 key inválida, 429 rate limit, sin red): pastilla gris con un icono y un tooltip explicativo, sin diálogos modales.

## Configuración
- API key: si no existe, se pide en el primer arranque con una ventana pequeña, también de estilo Fluent. Se guarda cifrada con DPAPI (CryptProtectData) en `%APPDATA%\VTDrop\config`. Nunca en texto plano ni en logs.
- El resto de la configuración va en un JSON en la misma carpeta: posición, siempre encima, tipo de fondo (acrylic / sistema / sólido), umbrales, tiempo de autocierre y opción de subida automática (desactivada por defecto).
- Inicio con Windows: clave `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` (sin necesidad de admin).
- La app NO debe requerir elevación (manifest `asInvoker`). Si se ejecuta elevada, el drag & drop desde el Explorador falla por UIPI.
- Instancia única (mutex con nombre). Si se lanza otra instancia, se trae al frente la existente.

## Entregables
1. Estructura del proyecto (`Cargo.toml`, `build.rs` con manifest e icono embebidos usando `winres` o `embed-resource`, módulos `ui/`, `vt/`, `hash/`, `config/`).
2. Código completo y compilable con `cargo build --release`.
3. README: cómo compilar, cómo obtener la API key y tamaño/RAM medidos.
4. Prueba de la parte VT con el hash de EICAR (`275a021bbfb6489e54d471899f7db9d1663fc695ec2fe2a2c4538aabf651fd0f`).

## Reglas
- Primero propone la arquitectura y la máquina de estados de la UI (Idle → Hover → Hashing → Querying → [AskUpload → Uploading → Polling] → Result → Idle). Espera mi OK antes de escribir todo el código.
- Desarrolla por fases y verifica que cada una compile y funcione antes de pasar a la siguiente: (1) ventana con fondo translúcido + arrastre + drop, (2) hash + consulta a VT, (3) subida + cola, (4) animaciones.
- No agregues funciones que no estén en esta especificación (nada de dashboard, historial ni métricas de CPU).
- Toda la comunicación con la UI entre hilos se hace mediante PostMessage con mensajes WM_APP. Nada de estado global mutable sin sincronizar.