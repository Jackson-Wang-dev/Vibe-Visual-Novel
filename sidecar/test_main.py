from sidecar.main import (
    DEFAULT_PLANNING_MODEL,
    DEFAULT_PRO_MODEL,
    Intent,
    append_intent_to_prompt,
    build_intent_prompt,
    extract_json_object,
    parse_generated_output,
)


def test_parse_generated_output_with_newchars():
    result = parse_generated_output('#NEWCHARS: [{"node":"N","bind":"n","folder":"N"}]\nshow("n", "default")')
    assert result.new_chars[0].bind == 'n'
    assert result.script.startswith('show')


def test_intent_prompt_constraints_cover_four_kinds():
    cases = {
        "from_scratch": "complete valid NovaScript file",
        "dialogue_edit": "Only edit spoken/narration text lines",
        "staging_effect": "function calls and `<| ... |>` blocks",
        "incremental_tweak": "minimal and local",
    }
    for kind, expected in cases.items():
        prompt = append_intent_to_prompt("BASE", Intent(kind=kind, target_scope="scope"))
        assert f"Kind: {kind}" in prompt
        assert expected in prompt

def test_extract_json_object_accepts_fenced_json():
    assert extract_json_object('```json\n{"kind":"incremental_tweak"}\n```')["kind"] == "incremental_tweak"

def test_analyze_intent_defaults_to_pro_model():
    assert DEFAULT_PLANNING_MODEL == DEFAULT_PRO_MODEL


def test_intent_prompt_requires_grounding():
    intent_prompt = build_intent_prompt('show("bg", "backgrounds/corridor")', '加雨天演出')
    assert "current target file content as the source of truth" in intent_prompt
