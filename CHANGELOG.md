# Cambios

## 0.4.0

Compilar sin grabar: StutterFree lee los shaders de los archivos del juego y planifica las pipelines él mismo.

- **Unity (DirectX 11):** archivos serializados y paquetes UnityFS (LZ4/LZMA); cada shader DX11 se compila una vez.
- **Unreal Engine 4/5:** `.pak` (v3-11) e IoStore `.utoc/.ucas` sin cifrar (Oodle, zlib, LZ4); bibliotecas
  `ShaderArchive-*.ushaderbytecode`; detección de la versión del motor y de DX11/DX12.
  - DX11: cada shader, y los pares de teselación (HS+DS) de cada material.
  - DX12: firmas raíz de Unreal reconstruidas (reglas de 4.20 a 5.8, a partir de los contadores de recursos de cada
    shader, con el checksum DXBC real) y pipelines de estado neutro: compute, VS, VS→PS y VS→GS(→PS) de cada material,
    cadenas globales, PS de material tras los VS globales y, en UE5, PS compartidos por muchos materiales.
  - Quedan fuera los shaders con un `[WaveSize]` que la GPU no ejecuta (NVIDIA: 32; AMD: 32-64).
- **FromSoftware:** archivos BHD5/BDT (RSA, AES, DCX, BND4).
  - Dark Souls III / Remastered (DX11), con pares de teselación. Las claves de DS3 se descargan una vez de las
    publicadas por UXM Selective Unpack.
  - Elden Ring / Nightreign (DX12): la firma raíz que lleva cada shader, reescrita a la versión 1.0 que crea el juego.
- Probado vía Proton: MECCHA CHAMELEON (UE 5.5, 38.426 pipelines), Dark Souls III (4.089), Elden Ring (161.387), todos
  sin fallos.
- **App:** cada tarjeta muestra qué se compiló desde los archivos ("From Game Files"); las carátulas ya no parpadean al
  hacer scroll; botón de Ko-fi; opción de escanear al abrir (también `settings --scan-on-startup on|off` en la CLI).
- **Más ligera:** el escaneo ya no guarda la lista de los millones de archivos de la caché de Mesa (818 MB → 173 MB de
  RAM, y más rápido); interfaz con OpenGL (`glow`); un solo ejecutable para la app y la línea de comandos. El AppImage
  pasa de 15,4 a 10,1 MB.
- `stutterfree-cli plan <juego>` muestra el plan sin compilar. `STUTTERFREE_WARM_LOG=<archivo>` guarda los errores de
  vkd3d-proton del warmer, y si el warmer se para antes de tiempo ahora se avisa.

## 0.3.0-beta

Reescritura en Rust de Shaderless 0.2.3-beta, con interfaz nativa (egui) y AppImage: detección de juegos (Steam,
Heroic, Lutris), modo Record, compilación desde la grabación vía Proton, gestión de cachés NVIDIA/Mesa/vkd3d.
