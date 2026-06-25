from sidecar.main import Intent, append_intent_to_prompt, parse_generated_output, route_after_analyze


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