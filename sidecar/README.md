# VVN Agents Sidecar

Prompt 7 moves the staging and codegen steps into this Python sidecar. VVN still owns the outer retry loop, deterministic validators, character registration, draft writes, reload/seek, version history, and summaries. For each generation attempt VVN sends the already-built prompt plus the DeepSeek API key to `POST /generate`; the sidecar runs a LangGraph graph (`analyze -> staging/codegen -> parse`): `analyze` uses structured `Intent` output to classify the request and route pure dialogue edits around staging, `staging` uses `deepseek-v4-pro` with structured `Plan` output when needed, `codegen` uses `deepseek-v4-flash`, and `parse` returns structured `{ new_chars, script }`.

At startup the sidecar binds `127.0.0.1:0`, prints the selected port as the first stdout line, and serves FastAPI on that local port. API keys are not stored in environment variables or bundled into the executable; VVN passes the key per request.

## Development

```bash
pip install -r sidecar/requirements.txt pyinstaller
pip install -r sidecar/requirements-dev.txt
python -m pytest sidecar/test_main.py
python sidecar/main.py
```

## Packaging

Build a one-file executable for each target platform and place it in `src-tauri/binaries/` with Tauri's sidecar naming convention:

```bash
python -m PyInstaller --onefile --name vvn-agents sidecar/main.py --distpath src-tauri/binaries --workpath sidecar/build --specpath sidecar
copy src-tauri\binaries\vvn-agents.exe src-tauri\binaries\vvn-agents-x86_64-pc-windows-msvc.exe
```

VVN declares `bundle.externalBin = ["binaries/vvn-agents"]`, so the checked-in Windows artifact is `src-tauri/binaries/vvn-agents-x86_64-pc-windows-msvc.exe`. Other platforms need their own PyInstaller build and target-triple filename. Unsigned binaries may be blocked by the OS until code signing is configured.