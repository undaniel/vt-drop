# Migrar el dibujo a `Windows.UI.Composition`

**Estado:** implementado como backend principal, con el renderer Direct2D + DirectComposition v1 como respaldo. **Pendiente de validación visual:** acrílico con la ventana inactiva y consumo de memoria en frío.

## Implementación (2026-11-16)

- `src/ui/composition.rs` contiene el backend nuevo: `DesktopWindowTarget`, árbol de visuales (`ContainerVisual`, `SpriteVisual`, `ShapeVisual`), recorte con `CompositionGeometricClip` + `CompositionRoundedRectangleGeometry`, y un `CompositionDrawingSurface` por cada texto/glyph (escudo, etiqueta, panel, badge, anillo, brillo).
- Las animaciones de expansión/colapso las ejecuta el compositor: `ScalarKeyFrameAnimation`/`Vector2KeyFrameAnimation`/`Vector3KeyFrameAnimation` con `CubicBezierEasingFunction(0,0,0,1)`, duraciones 250/167 ms y un `CompositionScopedBatch` que avisa a la ventana (`WM_APP_ANIM_DONE`) al colapsar.
- El anillo indeterminado es un `SpriteVisual` con el arco dibujado una vez y una animación de rotación infinita; el determinate redibuja el arco solo cuando cambia el porcentaje.
- El texto se redibuja únicamente cuando cambia: ratio (incluido el conteo de entrada), porcentaje, tiempo transcurrido, hover de botones o tema.
- Se activa automáticamente en Windows 10 2004+ (build 19041) y en Windows 11. El acrílico real (`CreateHostBackdropBrush` + `DWMWA_USE_HOSTBACKDROPBRUSH`) se usa en Windows 11 con transparencias activas; en el resto queda el fondo sólido del tema.
- Para comparar con el renderer anterior: `set VTDROP_RENDERER=legacy` antes de arrancar. Si la creación del compositor falla, la app recurre sola al renderer antiguo.
- `cargo test` incluye una prueba que crea un `DesktopWindowTarget` real sobre una ventana oculta y dibuja tres frames; así se validan en CI las firmas WinRT, el dispositivo WARP/hardware y las superficies, sin mostrar nada.

Lo que **no** se implementó de esta propuesta: la capa de ruido del acrílico (requiere un `CompositionEffectBrush`/Win2D) y la migración del menú y del diálogo de la API key a este árbol de visuales. Esos dos pasaron a controles nativos de Win32 (menú `HMENU` y diálogo con `EDIT`/`BUTTON`), no a Composition. Tampoco hay `ExpressionAnimation`: las posiciones se calculan en Rust y se animan con keyframes.

---

## En una frase

Es la opción más nativa posible: acrílico real dentro del círculo y animaciones que corre el propio compositor del sistema. A cambio, hay que reescribir unas 1 200 líneas del renderer y la app usaría algo más de memoria.

## Cómo funciona hoy

| Pieza | Implementación actual |
|---|---|
| Dibujo | Direct3D 11 (WARP) + Direct2D/DirectWrite en un swap chain transparente, mostrado con DirectComposition v1 (`IDCompositionDevice`) |
| Forma | Círculo/cápsula dibujado con alfa por píxel. DWM no añade marco, esquinas ni borde |
| Fondo | Opaco con el color del tema. **Sin acrílico**: el acrílico de DWM (`SetWindowCompositionAttribute` o `DWMWA_SYSTEMBACKDROP_TYPE`) solo cubre rectángulos |
| Animación | Nuestro propio bucle: un hilo espera a DWM (`DwmFlush`) e invalida la ventana. En `WM_PAINT`, la app calcula el valor de la curva y **vuelve a dibujar todo el frame** con Direct2D |
| Transiciones | La ventana cambia de tamaño una vez por transición; en cada frame solo se mueve el contenido |
| Coste | 0 % de CPU en reposo. Mientras anima, el hilo de UI dibuja cada frame en la CPU (WARP) |

Este modelo funciona y es liviano, con dos límites:

1. **No puede haber acrílico con forma redonda.** Por eso el widget pasó de acrílico a fondo opaco cuando se volvió circular.
2. **La fluidez depende del hilo de UI.** Si ese hilo se ocupa, por ejemplo con el menú contextual abierto, la animación se pausa. Además, cada frame cuesta CPU, poca pero no cero.

## Qué es `Windows.UI.Composition`

Es la capa visual sobre la que se construyen WinUI y el shell de Windows 11. En lugar de entregarle píxeles ya dibujados, la app le entrega un **árbol de visuales**: formas, pinceles, recortes y superficies. El compositor del sistema (DWM) los dibuja y **anima por su cuenta, en su propio hilo**, sincronizado con la pantalla.

Una app Win32 la usa sin XAML así:

1. `CreateDispatcherQueueController` en el hilo de UI.
2. `Compositor` + `ICompositorDesktopInterop::CreateDesktopWindowTarget(hwnd)`.
3. Un árbol de `ContainerVisual`, `SpriteVisual` y `ShapeVisual` colgado de ese target.

## Qué ganaríamos

- **Acrílico real dentro del círculo y de la cápsula.** Un `SpriteVisual` con pincel de *backdrop* desenfoca lo que hay detrás de la ventana: `Compositor.CreateHostBackdropBrush()` con `DWMWA_USE_HOSTBACKDROPBRUSH`, combinado con efectos de tinte y ruido como el acrílico de WinUI. Se recorta con un `CompositionGeometricClip` de forma `CompositionEllipseGeometry` o `CompositionRoundedRectangleGeometry`. Es el aspecto de los flyouts de Windows 11, pero con nuestra forma.
- **Animaciones ejecutadas por el compositor**:
  - `ScalarKeyFrameAnimation` / `Vector2KeyFrameAnimation` con `CubicBezierEasingFunction(0,0,0,1)`: la misma curva Fluent que usamos hoy, pero calculada por DWM.
  - `SpringVector3NaturalMotionAnimation` si algún día se quiere un rebote físico.
  - `ExpressionAnimation` para atar el radio, el recorte y la posición del texto al tamaño de la cápsula: la transformación de círculo a cápsula queda declarada una sola vez.
  - Corren a la frecuencia real del monitor (60/120/144 Hz) aunque el hilo de UI esté ocupado.
- **Menos trabajo propio**: desaparecen `FrameClock`, el cálculo de cada frame en `tick()`, la lógica de "redimensionar una vez por transición" y el redibujo completo por frame. Solo se redibuja cuando cambia el contenido: el ratio, el porcentaje o el tiempo.
- **Coherencia con el menú**: hoy el menú ya delega su entrada al compositor (DirectComposition v1). Con la migración, todo usaría el mismo mecanismo.

## Qué cuesta

### Código

| Área | Cambio |
|---|---|
| `ui/render.rs` (~1 200 líneas) | Se reescribe. El escudo, el anillo, los botones y el texto pasan a ser visuales; lo que siga necesitando Direct2D (texto, escudo) se dibuja en `CompositionDrawingSurface` mediante `ICompositionDrawingSurfaceInterop::BeginDraw` |
| `app.rs` | Se simplifica: fuera `transition`, `anchor()`, el cálculo de `expand` por frame y `tick()`. Entran "inicia animación X" y los callbacks de fin (`CompositionScopedBatch.Completed`) |
| `ui/animation.rs` | `FrameClock` y la curva calculada en la CPU desaparecen. El anillo indeterminado pasa a ser un `ShapeVisual` con `TrimStart`/`TrimEnd` animados |
| `Cargo.toml` | Nuevas features del crate `windows`: `UI_Composition`, `UI_Composition_Desktop`, `System` (DispatcherQueue), `Win32_System_WinRT_Composition`, `Graphics_Effects` |
| Menú y diálogo | Opcional; pueden quedarse en DirectComposition v1 |

**El texto no lo dibuja Composition.** DirectWrite sigue siendo necesario. La diferencia es que ese dibujo se hace solo cuando cambia el contenido, no en cada frame.

### Recursos (estimados, **sin medir**)

| Métrica | Hoy (medido) | Con Composition (estimado) |
|---|---|---|
| Tamaño del `.exe` | 1.61 MiB | +100–300 KB (bindings WinRT) |
| Memoria privada en frío | ~7.2 MB | +3–8 MB (Compositor, DispatcherQueue, CoreMessaging) |
| CPU en reposo | 0 % | 0 % (sin cambios) |
| CPU durante una animación | redibujo por frame en el hilo de UI | casi 0 en la app; el trabajo pasa a DWM |

El objetivo de la especificación (< 15 MB de RAM en reposo) probablemente se sigue cumpliendo en *working set*, pero hay que medirlo antes de decidir.

### Riesgos

- **Acrílico con la ventana inactiva.** El widget casi nunca tiene el foco. Hay que confirmar en la prueba inicial que el pincel de *host backdrop* sigue desenfocando aunque la ventana esté inactiva y no cae al color sólido de reserva. Si cae, la ganancia principal desaparece.
- **Windows 10.** `CreateHostBackdropBrush` y `DWMWA_USE_HOSTBACKDROPBRUSH` dependen de la versión. En Windows 10 habría que mantener el fondo opaco actual como reserva.
- **Hilo de UI con `DispatcherQueue`.** Es obligatorio y convive con nuestro message loop, los bucles modales del menú y del diálogo, y OLE drag & drop. Requiere pruebas.
- **WARP.** Hoy usamos WARP para ahorrar memoria del driver. Composition dibuja con el dispositivo de DWM, y las superficies de texto pueden seguir con nuestro dispositivo D2D/WARP mediante `CompositionGraphicsDevice`. Hay que verificar el impacto en memoria.
- **Hit-testing.** Sigue siendo Win32 (`WM_MOUSEMOVE`, `WM_NCHITTEST`). No cambia, pero el recorte visual y el área clicable deben coincidir.

## Plan por fases, si se decide hacerlo

1. **Prueba inicial (decisión sí/no)**, en una rama aparte:
   - Ventana con `DesktopWindowTarget`, un círculo con *host backdrop* y tinte.
   - Expansión a cápsula animada por el compositor.
   - Medir memoria privada y *working set* en frío y tras una animación, y CPU en reposo.
   - Comprobar el acrílico con la ventana **inactiva**.
   - Si algo de esto falla, se descarta la migración.
2. **Widget**: círculo, cápsula, anillo, porcentaje/tiempo y veredicto como visuales. Texto en `CompositionDrawingSurface`. Las mismas curvas y duraciones de hoy (250/167 ms).
3. **Limpieza**: eliminar `FrameClock`, `transition`, `anchor()` y el redibujo por frame. Mantener el fondo opaco como reserva para Windows 10 o con las transparencias desactivadas.
4. **Opcional**: menú y diálogo de la API key con el mismo árbol.

Cada fase se verifica con las pruebas actuales (`cargo test`) y con la prueba visual con mouse real: soltar, ✕ al pasar el mouse, cancelar y resultado.

## Criterios para decidir

Hacer la migración si se cumplen las tres:

- El acrílico se ve bien en el círculo **con la ventana inactiva**.
- La memoria privada en frío queda **≤ 15 MB** y la CPU en reposo sigue en **0 %**.
- El acrílico o la fluidez a más de 60 Hz se consideran parte del producto, no un detalle.

Si no, el renderer actual (Direct2D + DirectComposition v1) sigue siendo la opción más simple y liviana.
