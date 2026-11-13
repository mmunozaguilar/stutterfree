<p align="center"><img src="assets/stutterfree-256.png" alt="StutterFree" width="128"></p>

# StutterFree

**Adiós a los tirones por compilación de shaders en tus juegos de Proton.** StutterFree precompila los pipelines
DirectX 12 y los shaders DirectX 11 en la caché del driver **antes** de jugar, usando el mismo Proton / vkd3d-proton /
DXVK que el juego.

StutterFree es una versión modificada y reescrita en Rust de **[Shaderless](https://github.com/cyberps96/Shaderless)
0.2.3-beta**, que a su vez es el port no oficial para Linux de **[SCSKiller](https://github.com/BlueHeisenberg/SCSKiller)**.
No está afiliado ni respaldado por ninguno de los dos proyectos: los fallos de esta versión son nuestros.

## Estructura

| Ruta | Qué es | Original en C# |
|---|---|---|
| `crates/stutterfree-core` | Lógica: lanzadores, driver/GPU, Proton, grabador, caché NVIDIA/vkd3d, compilación | `SCSKiller.Linux.Core` + partes de `SCSKiller.Core` |
| `crates/stutterfree-cli` | La línea de comandos: biblioteca (`run`) y binario `stutterfree-cli` | `SCSKiller.Linux.Cli` |
| `crates/stutterfree-gui` | Binario `stutterfree`: interfaz nativa con egui (mismo diseño que la original), a la frecuencia real del monitor; con un comando (`list`, `compile`…) o `--cli` hace de línea de comandos | `SCSKiller.Linux.App` (Photino + HTML) |
| `native/` | Grabador `d3d12.dll` y warmer `shaderless_warm.exe` (C++, se ejecutan dentro de Proton) | `Native/` + `upstream/proxy` |
| `packaging/appimage` | `AppRun` y `.desktop` | `install-desktop.sh` |

Los ficheros que escribe (`~/.local/share/stutterfree`, que al primer arranque copia los datos de Shaderless: `settings.json`, `games-cache-state.json`, `games/<id>/recording.db`,
`warmed-keys.txt`…) usan el mismo formato que la versión en C#, así que se puede cambiar de una a otra.

## Estado del port (fase 1)

Portado y probado:

- Detección de juegos en Steam, Heroic (Epic, sideload, GOG) y Lutris (pga.db, Epic vía Lutris, juegos añadidos por exe), nativos y Flatpak.
- Detección de GPU (NVIDIA / Mesa RADV / ANV), estadísticas y borrado de caché.
- Resolución de la versión de Proton de cada juego (CompatToolMapping, `config_info`, `compatibilitytool.vdf`, Heroic, Lutris).
- **Modo Record** completo: instalar/quitar el grabador, importar `shaderless.db`, `shaderless.keys`, detección de anti-cheat (se niega a instalar junto a uno).
- **Compilación desde la grabación**: filtrado incremental, división en lotes de 40.000, warmer vía `proton runinprefix`, empaquetado de la caché NVIDIA en partes de <1,85 GB, fusión de cachés vkd3d, instalación donde el juego la lee, `user_settings.py` de Proton, detección de caché obsoleta (driver / Proton / juego actualizado).
- Monitor de salida de juegos (importa la grabación y, si lo activas, compila al cerrar el juego).
- CLI y GUI con las mismas acciones que el original.

**Fase 2 (en curso): compilar sin grabar.**

- **Hecho: Unity con DirectX 11.** Lee los shaders de los archivos del juego (archivos serializados y paquetes UnityFS con
  LZ4/LZMA) y compila cada uno con DXVK. Probado con V Rising (11.106 shaders), Wanderburg (10.176), Guildrun (3.313) y
  Rift of the NecroDancer (1.221, compilado de punta a punta sin grabación). Para verlo sin compilar:
  `stutterfree-cli plan <juego>`.
- **Hecho: Unreal Engine 4/5.** Abre los `.pak` (v3-11) y los contenedores IoStore `.utoc/.ucas` sin cifrar (Oodle,
  zlib, LZ4), extrae los shaders de `ShaderArchive-*.ushaderbytecode`, detecta la versión del motor y si el juego usa DX11
  o DX12. Los juegos DX11 se compilan como los de Unity. En los DX12 reconstruye las firmas raíz de Unreal (reglas de
  4.20 a 5.8, a partir de los contadores de recursos de cada shader) y planifica cada compute shader y, dentro de cada
  material, cada VS solo, cada par VS→PS y cada cadena VS→GS(→PS) cuyas firmas encajan, con estado neutro; también las
  cadenas globales, los PS de material detrás de los VS globales (Nanite) y, en UE5, los PS compartidos por muchos
  materiales. Deja fuera los shaders con un `[WaveSize]` que la GPU no ejecuta (NVIDIA: 32; AMD: 32-64). Probado con MECCHA
  CHAMELEON (UE 5.5) en una RX 9070 XT: 38.426 pipelines compiladas vía Proton, 0 fallidas.
  Back 4 Blood usa un formato de `.pak` propio que no se puede leer (el original tampoco).
- **Hecho: FromSoftware con DirectX 11** (Dark Souls III, Dark Souls: Remastered). Descifra las cabeceras `.bhd` de
  los archivos `.bdt` con las claves RSA públicas del juego (del exe; para DS3, que no las trae a la vista, las publicadas
  por UXM Selective Unpack, que se descargan una vez y se guardan en `games/<id>/archive.keys`), y saca los shaders de los
  `.shaderbnd.dcx` (DCX + BND4). Probado con Dark Souls III: 3.915 shaders y 174 pares de teselación (HS+DS del mismo material, emparejados por sus
  firmas) compilados vía Proton, 0 fallidos.
- **Hecho: Elden Ring (DirectX 12).** Mismos archivos (hash de nombres de 64 bits, Oodle; claves en su propio exe). Sus
  shaders llevan su firma raíz (RTS0 1.1), que se reescribe a la versión 1.0 que crea el juego; las pipelines salen de
  cada material (VS, VS→PS, VS→GS→PS) cuando todas sus etapas llevan la misma firma. Probado: 161.387 pipelines
  compiladas vía Proton, 0 fallidas. Elden Ring tiene anti-cheat, así que no se puede grabar: queda "Partial".
- **Pendiente:** en DX12, la teselación y los mesh shaders (ningún juego probado los usa) y las reglas de firma raíz de
  los forks de UE4 (Hogwarts Legacy, Jedi: Survivor, FF7 Rebirth);
  las librerías de ray tracing de Elden Ring y los lectores de RE Engine, REDengine, Dagor y Northlight; los pares de teselación de DirectX 11 en Unity.
  Lo que falte se cubre con una grabación: activa ● Record, juega 5-10 minutos, cierra el juego y pulsa Compile. El punto
  de enganche es `crates/stutterfree-core/src/engines.rs`; el código C# de referencia está en `reference/upstream/`.

## Compilar

Dependencias (Arch / CachyOS):

```bash
sudo pacman -S --needed rust mingw-w64-gcc squashfs-tools
paru -S appimagetool-bin
```

```bash
cargo test                 # tests del core
./build-native.sh          # d3d12.dll + shaderless_warm.exe con mingw
cargo run --release -p stutterfree         # la app
cargo run --release -p stutterfree-cli -- list
./build-appimage.sh        # dist/StutterFree-0.4.0-x86_64.AppImage
```

## Usar el AppImage

```bash
chmod +x StutterFree-0.4.0-x86_64.AppImage
./StutterFree-0.4.0-x86_64.AppImage                     # la app
./StutterFree-0.4.0-x86_64.AppImage --install-desktop   # añadirla al menú de aplicaciones
./StutterFree-0.4.0-x86_64.AppImage list                # la CLI: status, list, diagnose, settings, plan,
./StutterFree-0.4.0-x86_64.AppImage record "<juego>" on #   record, compile, compile-all, clear, clear-all
```

El AppImage no depende de WebKit ni de ninguna biblioteca extra: la interfaz es nativa (egui) y carga Wayland, X11 y
OpenGL del sistema al arrancar. Dibuja a la frecuencia real del monitor (144/180 Hz incluidos). Lleva un solo ejecutable
(la app y la línea de comandos), pesa unos 10 MB y ocupa unos 170 MB de RAM abierta.
Cada tarjeta muestra qué se compiló desde los archivos del juego ("From Game Files: 38,426 pipelines · Unreal 5.5
D3D12"). Escanear al abrir se puede apagar en Settings o con `stutterfree-cli settings --scan-on-startup off`.

Variables útiles para depurar: `STUTTERFREE_SCROLL_BENCH=1` (mide los fps del scroll y sale),
`STUTTERFREE_OPEN_DIALOG=settings|diagnose`, `STUTTERFREE_MAXIMIZED=1`, `STUTTERFREE_DATA_DIR=<carpeta>`,
`STUTTERFREE_WARM_LOG=<archivo>` (guarda los errores de vkd3d-proton del warmer: por qué falla una pipeline).

## Licencia

GPL-3.0-or-later con el permiso adicional de `LICENSE-EXCEPTION.txt`, igual que SCSKiller y Shaderless.
Copyright (C) 2026 The SCSKiller authors; Linux port (Shaderless) Copyright (C) 2026 cyberps96 and the Shaderless contributors;
Rust rewrite (StutterFree) Copyright (C) 2026 the StutterFree contributors.

StutterFree es gratis. Si te sirve, puedes apoyar su desarrollo en [Ko-fi](https://ko-fi.com/maritokura) (botón ☕ en la app). No pertenece a ningún fabricante de GPU, motor de juegos ni editora;
todas las marcas pertenecen a sus dueños.
