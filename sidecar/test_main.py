from sidecar.main import (
    DEFAULT_PLANNING_MODEL,
    DEFAULT_PRO_MODEL,
    Intent,
    append_intent_to_prompt,
    build_intent_prompt,
    build_staging_prompt,
    extract_json_object,
    parse_generated_output,
    route_after_analyze,
)


def test_parse_generated_output_with_newchars():
    result = parse_generated_output('#NEWCHARS: [{"node":"N","bind":"n","folder":"N"}]\nshow("n", "default")')
    assert result.new_chars[0].bind == 'n'
    assert result.script.startswith('show')


def test_intent_routing_respects_needs_staging():
    assert route_after_analyze({"intent": Intent(kind="staging_effect", target_scope="rain", needs_staging=True).model_dump()}) == "staging"
    assert route_after_analyze({"intent": Intent(kind="dialogue_edit", target_scope="line", needs_staging=False).model_dump()}) == "codegen"


def test_intent_prompt_constraints_cover_four_kinds():
    cases = {
        "from_scratch": "complete valid NovaScript file",
        "dialogue_edit": "Only edit spoken/narration text lines",
        "staging_effect": "function calls and `<| ... |>` blocks",
        "incremental_tweak": "minimal and local",
    }
    for kind, expected in cases.items():
        prompt = append_intent_to_prompt("BASE", Intent(kind=kind, target_scope="scope", needs_staging=False))
        assert f"Kind: {kind}" in prompt
        assert expected in prompt

def test_extract_json_object_accepts_fenced_json():
    assert extract_json_object('```json\n{"kind":"incremental_tweak","needs_staging":false}\n```')["kind"] == "incremental_tweak"

def test_planning_agents_default_to_pro():
    assert DEFAULT_PLANNING_MODEL == DEFAULT_PRO_MODEL


def test_router_and_director_prompts_require_grounding():
    intent_prompt = build_intent_prompt('show("bg", "backgrounds/corridor")', '加雨天演出')
    staging_prompt = build_staging_prompt('show("bg", "backgrounds/corridor")', '加雨天演出')
    assert "current target file content as the source of truth" in intent_prompt
    assert "concrete details already present in the current target file" in staging_prompt
