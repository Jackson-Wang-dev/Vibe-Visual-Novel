from __future__ import annotations

import json
import socket
from typing import Any, TypedDict

from fastapi import FastAPI, HTTPException
from pydantic import BaseModel, Field

try:
    from langchain_deepseek import ChatDeepSeek
    from langgraph.graph import END, StateGraph
except ImportError:  # pragma: no cover - lets the sidecar report a useful runtime error.
    ChatDeepSeek = None  # type: ignore[assignment]
    END = "__end__"  # type: ignore[assignment]
    StateGraph = None  # type: ignore[assignment]


NEW_CHARS_PREFIX = "#NEWCHARS:"
DEFAULT_MODEL = "deepseek-v4-flash"


class NewCharacterSpec(BaseModel):
    node: str
    bind: str
    folder: str


class GenerateRequest(BaseModel):
    prompt: str | None = None
    api_key: str | None = None
    model: str = DEFAULT_MODEL
    user_prompt: str | None = None
    target_file: str | None = None
    existing_content: str | None = None
    reference_md: str | None = None
    snapshot: dict[str, Any] | None = None
    vfx_notes: str | None = None


class GenerateResponse(BaseModel):
    new_chars: list[NewCharacterSpec] = Field(default_factory=list)
    script: str


class CodegenState(TypedDict, total=False):
    prompt: str
    api_key: str
    model: str
    raw_output: str
    new_chars: list[dict[str, str]]
    script: str


app = FastAPI(title="VVN Agents Sidecar")


def parse_generated_output(output: str) -> GenerateResponse:
    trimmed = output.strip()
    if not trimmed:
        raise ValueError("model returned an empty script")

    if trimmed.startswith(NEW_CHARS_PREFIX):
        rest = trimmed[len(NEW_CHARS_PREFIX) :]
        newline_index = rest.find("\n")
        if newline_index < 0:
            raise ValueError("#NEWCHARS header is missing following script text")
        json_text = rest[:newline_index].strip()
        script = rest[newline_index + 1 :].strip()
        if not script:
            raise ValueError("script text after #NEWCHARS is empty")
        return GenerateResponse(new_chars=[NewCharacterSpec.model_validate(item) for item in json.loads(json_text)], script=script)

    return GenerateResponse(script=trimmed)


def call_deepseek(state: CodegenState) -> CodegenState:
    if ChatDeepSeek is None:
        raise RuntimeError("langchain-deepseek is not installed")
    api_key = state.get("api_key", "").strip()
    if not api_key:
        raise RuntimeError("DeepSeek API Key is empty")
    llm = ChatDeepSeek(model=state.get("model") or DEFAULT_MODEL, api_key=api_key)
    response = llm.invoke(state["prompt"])
    content = getattr(response, "content", response)
    if isinstance(content, list):
        content = "\n".join(str(part.get("text", part)) if isinstance(part, dict) else str(part) for part in content)
    return {**state, "raw_output": str(content)}


def parse_codegen(state: CodegenState) -> CodegenState:
    parsed = parse_generated_output(state.get("raw_output", ""))
    return {
        **state,
        "new_chars": [char.model_dump() for char in parsed.new_chars],
        "script": parsed.script,
    }


def run_codegen_graph(prompt: str, api_key: str, model: str) -> GenerateResponse:
    if StateGraph is None:
        raise RuntimeError("langgraph is not installed")
    graph = StateGraph(CodegenState)
    graph.add_node("codegen", call_deepseek)
    graph.add_node("parse", parse_codegen)
    graph.set_entry_point("codegen")
    graph.add_edge("codegen", "parse")
    graph.add_edge("parse", END)
    result = graph.compile().invoke({"prompt": prompt, "api_key": api_key, "model": model})
    return GenerateResponse(
        new_chars=[NewCharacterSpec.model_validate(item) for item in result.get("new_chars", [])],
        script=result["script"],
    )


@app.post("/generate", response_model=GenerateResponse)
def generate(request: GenerateRequest) -> GenerateResponse:
    prompt = request.prompt or request.user_prompt or ""
    if not prompt.strip():
        raise HTTPException(status_code=400, detail="prompt is empty")
    try:
        return run_codegen_graph(prompt, request.api_key or "", request.model or DEFAULT_MODEL)
    except Exception as error:  # LangGraph/LLM errors should surface cleanly to Rust.
        raise HTTPException(status_code=500, detail=str(error)) from error


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