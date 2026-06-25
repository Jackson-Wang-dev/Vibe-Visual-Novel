# VVN Agents Sidecar

Prompt 5 introduces a minimal Python sidecar process for the future multi-agent pipeline. The current service intentionally behaves as an echo bridge: VVN calls `POST /generate`, ignores the echo response, and then continues through the existing Rust DeepSeek generation path so user-visible behavior stays unchanged.

At startup the sidecar binds `127.0.0.1:0`, prints the selected port as the first stdout line, and serves FastAPI on that local port.

## Development

```bash
pip install -r sidecar/requirements.txt pyinstaller
python sidecar/main.py
```

## Packaging

Build a one-file executable for each target platform and place it in `src-tauri/binaries/` with Tauri's sidecar naming convention:

```bash
python -m PyInstaller --onefile --name vvn-agents sidecar/main.py --distpath src-tauri/binaries --workpath sidecar/build --specpath sidecar
copy src-tauri\binaries\vvn-agents.exe src-tauri\binaries\vvn-agents-x86_64-pc-windows-msvc.exe
```

VVN declares `bundle.externalBin = ["binaries/vvn-agents"]`, so the checked-in Windows artifact is `src-tauri/binaries/vvn-agents-x86_64-pc-windows-msvc.exe`. Other platforms need their own PyInstaller build and target-triple filename. Unsigned binaries may be blocked by the OS until code signing is configured.