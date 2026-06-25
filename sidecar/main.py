from __future__ import annotations

import json
import socket
from typing import Any, TypedDict

from fastapi import FastAPI, HTTPException
from pydantic import BaseModel, ConfigDict, Field

try:
    from langchain_deepseek import ChatDeepSeek
    from langgraph.graph import END, StateGraph
except ImportError:  # pragma: no cover - lets the sidecar report a useful runtime error.
    ChatDeepSeek = None  # type: ignore[assignment]
    END = "__end__"  # type: ignore[assignment]
    StateGraph = None  # type: ignore[assignment]


NEW_CHARS_PREFIX = "#NEWCHARS:"
DEFAULT_FLASH_MODEL = "deepseek-v4-flash"
DEFAULT_PRO_MODEL = "deepseek-v4-pro"

STAGING_FUNCTIONS = """## Available NovaScript staging functions

- Sound: `play(channel, track_name, vol, duration)`, `sound(track_name, vol)`
- Text presentation: `set_box(pos_name, style_name)`, `set_text_appear(mode, char_speed, fade_duration)`, `text_delay(time)`, `box_hide_show(duration, pos_name, style_name)`
- Visual color: `tint(obj, color, duration, entry)`, `env_tint(obj, color, duration, entry)`
- Visual animation: `move(obj, coord, scale, angle, duration, entry)`, `wait(duration, entry)`, `anim_hold_begin()`, `anim_hold_end()`, `cam_punch(entry)`
- Visual VFX: `vfx(obj, shader_layer, t, duration, properties, entry)`
"""


class NewCharacterSpec(BaseModel):
    model_config = ConfigDict(populate_by_name=True)

    node: str
    bind: str
    folder: str


class Plan(BaseModel):
    model_config = ConfigDict(populate_by_name=True)

    sound: str = ""
    text_presentation: str = ""
    visual_color: str = ""
    visual_animation: str = ""
    visual_vfx: str = ""

    def is_empty(self) -> bool:
        return not any(
            field.strip()
            for field in [
                self.sound,
                self.text_presentation,
                self.visual_color,
                self.visual_animation,
                self.visual_vfx,
            ]
        )


class GenerateRequest(BaseModel):
    model_config = ConfigDict(populate_by_name=True)

    prompt: str | None = None
    api_key: str | None = Field(default=None, alias="apiKey")
    model: str = DEFAULT_FLASH_MODEL
    planning_model: str = Field(default=DEFAULT_PRO_MODEL, alias="planningModel")
    user_prompt: str | None = Field(default=None, alias="userPrompt")
    target_file: str | None = Field(default=None, alias="targetFile")
    existing_content: str | None = Field(default=None, alias="existingContent")
    reference_md: str | None = Field(default=None, alias="referenceMd")
    snapshot: dict[str, Any] | None = None
    vfx_notes: str | None = Field(default=None, alias="vfxNotes")


class GenerateResponse(BaseModel):
    model_config = ConfigDict(populate_by_name=True)

    new_chars: list[NewCharacterSpec] = Field(default_factory=list, alias="newChars")
    script: str


class CodegenState(TypedDict, total=False):
    prompt: str
    codegen_prompt: str
    api_key: str
    model: str
    planning_model: str
    user_prompt: str
    existing_content: str
    raw_output: str
    plan: dict[str, str]
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


def build_staging_prompt(existing_content: str, user_prompt: str) -> str:
    existing = existing_content.strip() or "(empty - this is a new file)"
    return f"""You are the staging director for a Nova2 visual novel scene. Analyze the user's request across five dimensions: sound, text presentation, visual color, visual animation, and visual VFX.

Return structured fields only. If a dimension is irrelevant, leave it as an empty string. Do not invent work just to fill fields. Describe intent and likely function calls briefly; do not write full NovaScript here.

{STAGING_FUNCTIONS}

## Current target file content

{existing}

## User request

{user_prompt}"""


def append_plan_to_prompt(prompt: str, plan: Plan) -> str:
    if plan.is_empty():
        return prompt
    lines: list[str] = []
    if plan.sound.strip():
        lines.append(f"- Sound: {plan.sound.strip()}")
    if plan.text_presentation.strip():
        lines.append(f"- Text presentation: {plan.text_presentation.strip()}")
    if plan.visual_color.strip():
        lines.append(f"- Visual color: {plan.visual_color.strip()}")
    if plan.visual_animation.strip():
        lines.append(f"- Visual animation: {plan.visual_animation.strip()}")
    if plan.visual_vfx.strip():
        lines.append(f"- Visual VFX: {plan.visual_vfx.strip()}")
    plan_section = "\n".join(lines)
    return (
        f"{prompt}\n\n"
        "## Staging plan for this request\n\n"
        "Apply every relevant item below as concrete NovaScript function calls. Do not implement only one dimension if multiple are listed.\n"
        f"{plan_section}"
    )


def make_deepseek(model: str, api_key: str):
    if ChatDeepSeek is None:
        raise RuntimeError("langchain-deepseek is not installed")
    api_key = api_key.strip()
    if not api_key:
        raise RuntimeError("DeepSeek API Key is empty")
    return ChatDeepSeek(model=model, api_key=api_key)


def stage_plan(state: CodegenState) -> CodegenState:
    user_prompt = state.get("user_prompt", "").strip()
    if not user_prompt:
        return {**state, "codegen_prompt": state["prompt"], "plan": {}}
    llm = make_deepseek(state.get("planning_model") or DEFAULT_PRO_MODEL, state.get("api_key", ""))
    structured_llm = llm.with_structured_output(Plan)
    plan = structured_llm.invoke(build_staging_prompt(state.get("existing_content", ""), user_prompt))
    if not isinstance(plan, Plan):
        plan = Plan.model_validate(plan)
    return {
        **state,
        "plan": plan.model_dump(),
        "codegen_prompt": append_plan_to_prompt(state["prompt"], plan),
    }


def call_deepseek(state: CodegenState) -> CodegenState:
    llm = make_deepseek(state.get("model") or DEFAULT_FLASH_MODEL, state.get("api_key", ""))
    response = llm.invoke(state.get("codegen_prompt") or state["prompt"])
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


def run_codegen_graph(request: GenerateRequest) -> GenerateResponse:
    if StateGraph is None:
        raise RuntimeError("langgraph is not installed")
    prompt = request.prompt or ""
    if not prompt.strip():
        raise RuntimeError("prompt is empty")
    graph = StateGraph(CodegenState)
    graph.add_node("staging", stage_plan)
    graph.add_node("codegen", call_deepseek)
    graph.add_node("parse", parse_codegen)
    graph.set_entry_point("staging")
    graph.add_edge("staging", "codegen")
    graph.add_edge("codegen", "parse")
    graph.add_edge("parse", END)
    result = graph.compile().invoke(
        {
            "prompt": prompt,
            "api_key": request.api_key or "",
            "model": request.model or DEFAULT_FLASH_MODEL,
            "planning_model": request.planning_model or DEFAULT_PRO_MODEL,
            "user_prompt": request.user_prompt or "",
            "existing_content": request.existing_content or "",
        }
    )
    return GenerateResponse(
        new_chars=[NewCharacterSpec.model_validate(item) for item in result.get("new_chars", [])],
        script=result["script"],
    )


@app.post("/generate", response_model=GenerateResponse)
def generate(request: GenerateRequest) -> GenerateResponse:
    if not (request.prompt or "").strip():
        raise HTTPException(status_code=400, detail="prompt is empty")
    try:
        return run_codegen_graph(request)
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