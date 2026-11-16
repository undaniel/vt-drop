# VTDrop

Widget flotante, sin instalador y sin privilegios de administrador, para escanear archivos con VirusTotal. Arrastra uno o varios archivos sobre el círculo del escudo: VTDrop calcula el SHA-256, consulta el informe y muestra el ratio de detecciones en una cápsula. Un clic en la cápsula abre el informe en el navegador.

## Estado actual

- **Forma propia**: círculo en reposo que se transforma en cápsula al mostrar un resultado. Se dibuja con alfa por píxel sobre `Windows.UI.Composition` (o Direct2D + DirectComposition v1 como respaldo). DWM no añade esquinas, borde ni fondo. Al pasar el mouse o un archivo por encima, el contorno toma el color de acento. En Windows 11 con transparencias activas, el círculo y la cápsula tienen acrílico real (`CreateHostBackdropBrush`).
- **Motion Fluent**: curva de WinUI `cubic-bezier(0,0,0,1)` y duraciones de WinUI: 250 ms al expandir, 167 ms al colapsar y en los hover. El texto entra después del panel con fundido y un deslizamiento de 8 px. El ratio cuenta hacia arriba y el color pasa del acento al verde, ámbar o rojo. El anillo de progreso imita al de WinUI: el arco gira, y en modo indeterminado lo hace el propio compositor.
- **Animaciones del compositor**: la expansión, el colapso, el hover y el anillo los ejecuta DWM en su hilo, a la frecuencia real del monitor (60/120/144 Hz), aunque el hilo de UI esté ocupado con el menú. En reposo no hay timers ni hilos de animación.
- **Una sola redimensión por transición**: al expandir o colapsar, la ventana toma una vez su tamaño final. Después solo se animan visuales, sin `SetWindowPos`.
- **Soltar archivos en cualquier momento**: en reposo, con un resultado o un error visible, mientras escanea o mientras pregunta si subir. Durante un escaneo, los archivos nuevos se suman al lote en curso y el contador crece al instante (`1/2`, `2/3`…).
- **Cancelar**: al pasar el mouse sobre el círculo mientras escanea aparece un ✕; un clic cancela. También desde el menú contextual (**Cancelar análisis**). El hash y la subida se cortan a mitad de lectura.
- **Progreso real**:
  - `42%` en el centro al calcular el hash de archivos ≥ 16 MB y al subir.
  - Tiempo transcurrido (`1:05`) mientras VirusTotal analiza una subida, porque ahí no existe un porcentaje.
  - El tooltip explica en qué paso va.
- **Resultado que no se pierde**: la cápsula solo se cierra tras `auto_close_secs` en los que estuviste usando el PC y sin el mouse encima. Si el análisis termina mientras no estás, sigue visible al volver. Debajo del ratio aparece el veredicto: *Sin detecciones*, *Sospechoso* o *Malicioso*.
- **VirusTotal**:
  - Límite de ventana deslizante: como máximo N peticiones en cualquier minuto, conservado entre lotes. La consulta y la subida salen seguidas, y varios archivos conocidos se resuelven a la vez.
  - Reintento ante 429 respetando `Retry-After`.
  - Los archivos subidos se revisan en segundo plano cada 15 s, durante un máximo de 10 min, mientras se procesan los siguientes.
  - Los archivos de más de 650 MB se rechazan antes de pedir la subida.
- **Sistema**:
  - API key cifrada con DPAPI.
  - Instancia única, inicio con Windows y toasts de resumen.
  - DPI PerMonitorV2.
  - Tema de apps claro/oscuro (`AppsUseLightTheme`) y color de acento, con cambio en caliente.
  - Respeta "Efectos de animación" de Windows.

Medido con el renderer de respaldo en Windows 11 26300 (x64, 1920×1080 al 100 %, iGPU Intel, WARP), 2026-11-16. El backend de `Windows.UI.Composition` está pendiente de medir (ver `MIGRACION_COMPOSITION.md`):

| Métrica | Valor |
|---|---|
| `vtdrop.exe` release | **1.61 MiB** (1 689 088 bytes) |
| Working set en reposo | **~0.3–0.4 MB** tras el trim |
| Private bytes en frío | **~7.2 MB** |
| Private bytes tras un escaneo | **~14–17 MB** (TLS y WinRT cargados) |
| Working set con un resultado visible | ~40 MB (superficies de WARP/DComp); vuelve a < 1 MB al cerrarse |
| CPU en reposo | **0 ms** en 10 s, también con un resultado visible |
| CPU de un escaneo de archivo conocido | ~0.1 s en total (arranque, hash, consulta y expansión) |
| Dependencias externas | Solo DLLs del sistema (D3D11/WARP, D2D1, DWrite, DWM, comctl32…) |

> El renderer de respaldo elige WARP primero porque el driver de la GPU añadía por sí solo decenas de MB de memoria privada en esta máquina. El backend de composición usa el dispositivo de hardware, porque `CreateGraphicsDevice` trabaja con el mismo dispositivo Direct2D que dibuja las superficies de texto.

## Compilar

### Rust y toolchain

Probado con Rust stable 1.100 (2026-11) y Windows 11.

**GNU (sin administrador; modo usado para las medidas):**

1. Instala Rust GNU (`stable-x86_64-pc-windows-gnu`) con rustup.
2. Instala WinLibs (GCC + binutils + `windres`) y añade su `bin` al `PATH`.
3. En esta máquina ambas rutas ya están en el `PATH` de usuario:
   - `%USERPROFILE%\.cargo\bin`
   - `%LOCALAPPDATA%\Microsoft\WinGet\Packages\BrechtSanders.WinLibs.POSIX.UCRT_...\mingw64\bin`

```powershell
cargo build --release
.\target\release\vtdrop.exe
```

**MSVC:**

Instala Visual Studio Build Tools con "Desarrollo para el escritorio con C++" y Windows SDK, y compila:

```powershell
rustup default stable-x86_64-pc-windows-msvc
cargo build --release
```

`build.rs` incrusta el icono y el manifest con `embed-resource`. En GNU se incrusta solo el icono, porque MinGW-W64 14 añade su propio manifest `longPathAware` y el linker descarta uno de los dos si se incrusta otro. Por eso, en GNU el DPI se activa con `SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2)` antes de crear la ventana. En MSVC el manifest incluye DPI, `asInvoker`, long paths y Common Controls v6. El comportamiento es el mismo.

### Tamaño y perfiles

`Cargo.toml` usa `opt-level="z"`, LTO, `codegen-units=1`, `strip=true`, `panic="abort"` e `incremental=false`.

## API key

1. Crea una cuenta en <https://www.virustotal.com/> y copia la clave en <https://www.virustotal.com/gui/my-apikey>.
2. En el primer arranque, VTDrop pide la clave en una ventana estilo Fluent: acrílico, Segoe UI Variable, campo redondeado y aviso si la clave no es válida. Admite `Ctrl+V`, flechas, `Supr` y `Enter`.
3. Se guarda cifrada con DPAPI en `%APPDATA%\VTDrop\config` (blob binario ligado al usuario). El resto de ajustes está en `%APPDATA%\VTDrop\config.json`.
4. La clave nunca se escribe en JSON, logs ni argumentos de proceso. Para cambiarla: clic derecho → **Configurar API key…**.

La API pública de VirusTotal admite 4 peticiones por minuto (y 500 al día). El valor por defecto de VTDrop es 4 y se cambia en el JSON (`requests_per_minute`).

## Uso

- **Arrastrar archivos** desde el Explorador sobre el círculo. Se aceptan varios y en cualquier momento. Mientras escanea, el círculo brilla y los archivos se suman al lote.
- **Clic en el resultado**: abre `https://www.virustotal.com/gui/file/<sha256>`.
- **Clic en el círculo mientras escanea**: cancela. El ✕ aparece al pasar el mouse.
- **Clic derecho**: menú estilo Windows 11 con *Cancelar análisis* (solo mientras escanea), *Siempre encima*, *Iniciar con Windows*, *Configurar API key…*, *Abrir último resultado* y *Salir*. Entra con fundido y deslizamiento hechos por el compositor.
- **Arrastrar el círculo**: mueve el widget. La posición se guarda al soltar.
- **Línea de comandos**: `vtdrop.exe <archivo>…` analiza esos archivos al arrancar (sirve para "Abrir con"). Si VTDrop ya está abierto, los argumentos se ignoran.
- **Carpetas**: se ignoran por defecto. Para recorrerlas, pon `"scan_folders": true` en `config.json`.
- **Subida**: si VirusTotal no conoce el archivo (404), aparece una cápsula con ✓/✕ y el aviso de que el archivo será visible para otros usuarios de VirusTotal. Nunca se sube sin confirmar. `"auto_upload": true` es un opt-in explícito que evita la pregunta.
- **Pasar el mouse** mientras escanea o sobre un error muestra un tooltip con el paso actual o el detalle del error.

### ¿Por qué tarda a veces?

- **Archivo que VirusTotal ya conoce**: el resultado llega en menos de 1 s (consulta medida: ~0.5 s).
- **Archivo nuevo**: hay que subirlo y esperar el análisis. Con la API pública esos análisis van a una cola de baja prioridad y pueden tardar uno o varios minutos. VTDrop revisa cada 15 s, lo máximo que permite la cuota de 4 peticiones por minuto, y muestra el tiempo transcurrido.
- **Archivo grande con antivirus**: puede quedarse un par de segundos con el anillo antes del `%`. Es el antivirus de Windows revisando el archivo al abrirlo.

Con varios archivos, el contador muestra `2/5`. Al final se muestra el peor ratio y un toast lista cada archivo con su resultado (hasta 8 líneas; si hay más, añade "y N más").

## Configuración (`%APPDATA%\VTDrop\config.json`)

```json
{
  "position": [1832, 944],
  "always_on_top": true,
  "threshold_amber": 1,
  "threshold_red": 4,
  "auto_close_secs": 8,
  "auto_upload": false,
  "scan_folders": false,
  "requests_per_minute": 4,
  "version": 1
}
```

- `threshold_amber` / `threshold_red`: detecciones (malicious + suspicious) que dan ámbar y rojo. Por defecto: 0 → verde, 1–3 → ámbar, 4 o más → rojo.
- `auto_close_secs`: segundos de uso del PC antes de cerrar el resultado. Con el mouse encima, o si no estás usando el PC, no corre. `0` mantiene la cápsula abierta.
- `requests_per_minute`: 1–60. 4 es el límite público.
- `position`: esquina superior izquierda física del círculo. Se ajusta al área de trabajo del monitor.

La opción `backdrop` y la tecla F9 que la alternaba ya no existen: el widget dibuja su propia forma y el fondo lo compone la app. En Windows 11 con transparencias activas usa acrílico real dentro del círculo; en Windows 10 o con transparencias desactivadas, el color sólido del tema. Un `config.json` antiguo que incluya `backdrop` sigue cargando sin problema. El menú y el diálogo de la API key siguen usando DirectComposition v1.

## Arquitectura

```
main.rs / lib.rs      WinMain, mutex de instancia, COM/OLE, argumentos, message loop
app.rs                máquina de estados UI, transiciones, hit-testing, menú, cancelación
ui/window.rs          DWM: sin marco ni bordes, tema, topmost, DPI, host backdrop
ui/composition.rs     Windows.UI.Composition: círculo/cápsula, acrílico, animaciones, superficies D2D
ui/render.rs          respaldo: D3D11 (WARP) + DXGI + DComposition + D2D/DWrite
ui/animation.rs       curva Fluent, anillo WinUI, FrameClock solo para el respaldo
ui/menu.rs            menú contextual propio estilo Windows 11
ui/menu_render.rs     dibujo del menú sobre la superficie compartida
ui/surface.rs         superficie D3D11/D2D/DComp compartida + entrada animada por el compositor
ui/dialog_render.rs   diálogo de API key Fluent
ui/tooltip.rs         tooltip propio (GDI) con fundido de entrada
drop_target.rs        IDropTarget OLE (CF_HDROP) + fallback WM_DROPFILES
worker.rs             hilo único: cola, hash, consultas, subida, análisis en segundo plano, cancelación
vt/mod.rs             cliente VT API v3 (rustls), límite de ventana deslizante, reintento 429
hash.rs               SHA-256 en streaming (buffer de 1 MB) con progreso y corte
config/               JSON atómico + DPAPI + diálogo de API key
toast.rs              toast WinRT con AUMID y acceso directo en Inicio
```

### Máquina de estados

```
Idle ──DragEnter──▶ Dragging ──Drop──▶ Busy(Hashing ─▶ Querying)
  ▲                                       │        │
  │                         404 ──────────┘        ├─ conocido ──▶ Result
  │                          ▼                     │
  │                     AskUpload ──✓──▶ Busy(Uploading ─▶ Polling en segundo plano)
  │                          │ ✕                                  │
  │                          └──────────▶ siguiente archivo ◀─────┘
  │
  ├──◀── autocierre (solo con el usuario presente) ◀── Result / Error
  └──◀── Cancelar (✕ o menú) ◀── Busy / AskUpload

Drop en Busy/AskUpload → se suma al lote (Queued).  Drop en Result/Error → lote nuevo.
401/429/red → Error (cápsula gris + tooltip)
```

La UI nunca bloquea: el worker es dueño de la cola y de la red. La comunicación worker→UI es una cola `Mutex<VecDeque>` + `PostMessage(WM_APP_EVENT)`. La UI→worker usa un canal `mpsc`. La cancelación viaja como bandera atómica, para cortar el hash o la subida en el acto, y además como mensaje por el canal: todo lo que llega antes de ese mensaje pertenece al lote cancelado y se descarta.

### Render y DPI

El backend principal es `Windows.UI.Composition`. La ventana tiene un `DesktopWindowTarget` y un árbol de visuales que DWM dibuja y anima: el recorte de círculo/cápsula es un `CompositionGeometricClip`, el fondo es un `CreateHostBackdropBrush` con tinte, y cada texto (ratio, porcentaje, tiempo, avisos, botones, badge) vive en un `CompositionDrawingSurface` que solo se redibuja cuando cambia su contenido. Las animaciones de expansión/colapso usan keyframes con la curva Fluent y un `CompositionScopedBatch` avisa al colapsar.

La ventana toma una sola vez el tamaño final por transición; después nada llama a `SetWindowPos`. Las coordenadas de composición van en píxeles físicos y las superficies se crean a `DPI / 96`, así que el texto queda nítido a cualquier escala. Al cambiar de monitor se recrea el renderer.

En Windows 10 2004+ funciona el mismo backend con fondo sólido; el acrílico requiere Windows 11 y transparencias activas. Si `CreateDesktopWindowTarget` falla, `ui/mod.rs` cae al renderer antiguo (D3D11 WARP + DirectComposition v1 + Direct2D) sin cambiar el resto de la app. Para forzarlo: `VTDROP_RENDERER=legacy`. Ver [MIGRACION_COMPOSITION.md](MIGRACION_COMPOSITION.md).

## Pruebas

36 pruebas unitarias. Cubren hash EICAR, estadísticas, configuración, curvas de animación, continuidad del anillo, geometría del compositor (círculo → cápsula, ancla y glyph), límite de peticiones, cola del lote y cancelación sin fugas al lote siguiente. Una de ellas crea un `DesktopWindowTarget` real sobre una ventana oculta y dibuja tres frames con el compositor.

```powershell
cargo test --release
```

Prueba en vivo contra VirusTotal con el hash de EICAR:

```
275a021bbfb6489e54d471899f7db9d1663fc695ec2fe2a2c4538aabf651fd0f
```

Con una key en el entorno:

```powershell
$env:VT_API_KEY = "..."
cargo test --test live_vt -- --ignored --nocapture
```

O con la key DPAPI que haya guardado la app. Sin argumentos usa EICAR, y también acepta cualquier SHA-256:

```powershell
cargo run --release --example vt_probe
cargo run --release --example vt_probe -- <sha256>
```

El probe solo consulta, no sube nada. Resultado real (2026-11-16, API v3):

```
EICAR SHA-256: 275a021bbfb6489e54d471899f7db9d1663fc695ec2fe2a2c4538aabf651fd0f
stats: Stats { malicious: 65, suspicious: 0, undetected: 2, harmless: 0 }
ratio: 65/67
```

Verificado con mouse y arrastres reales y capturas de pantalla (2026-11-16):

- `notepad.exe` → `0/71 · Sin detecciones` ~1 s después de soltarlo.
- Archivo de 1.5 GB → `0%` … `30%` mientras calcula el hash; luego *Demasiado grande*, sin subir nada.
- Soltar `notepad.exe` sobre el círculo mientras escanea dos archivos grandes → `Sin detecciones · peor de 3`.
- Escaneo de 3 GB → ✕ al pasar el mouse → clic → reposo sin errores. Soltar otro archivo después da su resultado normal.

## Seguridad y privacidad

- Manifest MSVC `asInvoker`; no requiere elevación. Si se ejecutara elevado, el drag & drop desde el Explorador fallaría por UIPI.
- La API key se cifra con `CryptProtectData` (DPAPI, entropía propia) y nunca se registra.
- En las consultas normales solo se envía el hash. La subida (solo tras confirmar) envía el archivo completo, y VirusTotal lo hace visible a otros usuarios.
- HTTP con rustls; sin telemetría propia.
- Instancia única mediante un mutex con nombre local; una segunda instancia trae al frente la existente.

## Notas

- El toast requiere un acceso directo `%APPDATA%\Microsoft\Windows\Start Menu\Programs\VTDrop.lnk` con AUMID `VTDrop.Desktop`. VTDrop lo crea o actualiza al arrancar. Si Windows bloquea los toasts, el resumen sigue visible en la cápsula.
- El menú contextual es un popup propio (`ui/menu.rs`), no un `HMENU`: `TrackPopupMenu` siempre dibuja el menú clásico cuadrado, y el menú redondeado del Explorador es un `MenuFlyout` de XAML/WinUI. Usa el mismo pipeline DirectComposition + Direct2D, con acrílico de DWM, Segoe UI Variable, hover y teclado, y sigue el tema claro/oscuro.
- El diálogo de la API key (`config/key_dialog.rs` + `ui/dialog_render.rs`) tampoco usa `EDIT`/`BUTTON` nativos. La clave se muestra enmascarada y se valida localmente antes de cifrarla.
- **Diferencias con la especificación original** (`GUIA.md`):
  - El widget usa acrílico real dentro del círculo solo en Windows 11 con transparencias activas; en Windows 10 queda el fondo sólido del tema (DWM solo ofrece acrílico rectangular).
  - La expansión usa la curva Fluent en vez de un rebote tipo spring.
  - Tras subir un archivo, el análisis se revisa cada 15 s en lugar de con backoff.
  - Se quitaron la opción `backdrop` y la tecla F9.
  - Se añadieron cancelar, progreso y soltar durante el escaneo, que se pidieron después.
- Pendiente: alto contraste, navegación por teclado del widget y soporte para el Narrador.

## Licencia

MIT. Ver `LICENSE`.
