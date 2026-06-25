from __future__ import annotations

import socket
from typing import Any

from fastapi import FastAPI
from pydantic import BaseModel


class GenerateRequest(BaseModel):
    prompt: str | None = None
    user_prompt: str | None = None
    target_file: str | None = None
    existing_content: str | None = None
    reference_md: str | None = None
    snapshot: dict[str, Any] | None = None
    vfx_notes: str | None = None


app = FastAPI(title="VVN Agents Sidecar")


@app.post("/generate")
def generate(request: GenerateRequest) -> dict[str, Any]:
    # Prompt 5 deliberately keeps the sidecar as an echo bridge. VVN still calls the existing
    # Rust DeepSeek path after this handshake, so generation behavior stays unchanged.
    return {"prompt": request.prompt or request.user_prompt or ""}


def pick_free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as probe:
        probe.bind(("127.0.0.1", 0))
        return int(probe.getsockname()[1])


def main() -> None:
    import uvicorn

    port = pick_free_port()
    print(port, flush=True)
    uvicorn.run(app, host="127.0.0.1", port=port, log_level="warning")


if __name__ == "__main__":
    main()