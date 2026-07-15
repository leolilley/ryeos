use serde_json::{json, Value};

use super::*;

fn expression(source: &str) -> CompiledExpression {
    compile_expression(source, &CompilationLimits::default()).unwrap()
}

fn evaluate_source(source: &str, context: &Value) -> Value {
    evaluate(&expression(source), context, &EvaluationLimits::default()).unwrap()
}

fn render(source: &str, context: &Value) -> Value {
    compile_and_render(
        source,
        context,
        &CompilationLimits::default(),
        &EvaluationLimits::default(),
    )
    .unwrap()
}

#[test]
fn arithmetic_precedence_unary_and_ternary_are_conventional() {
    let context = json!({});
    assert_eq!(evaluate_source("1 + 2 * 3", &context), json!(7));
    assert_eq!(evaluate_source("-(2 + 3) * +4", &context), json!(-20));
    assert_eq!(evaluate_source("true ? 4 : 9", &context), json!(4));
    assert_eq!(
        evaluate_source("false ? missing.path : 9", &context),
        json!(9)
    );
}

#[test]
fn arithmetic_is_checked_and_strictly_typed() {
    let context = json!({});
    assert_eq!(evaluate_source("7 / 2", &context), json!(3.5));
    assert_eq!(evaluate_source("-7 % 3", &context), json!(-1));
    assert_eq!(evaluate_source("'ab' + 'cd'", &context), json!("abcd"));
    assert_eq!(
        evaluate_source("9223372036854775807 + 1", &context),
        json!(9223372036854775808u64)
    );
    assert_eq!(
        evaluate_source("9223372036854775807 * 2", &context),
        json!(18446744073709551614u64)
    );
    assert_eq!(
        evaluate_source("-9223372036854775808 * -1", &context),
        json!(9223372036854775808u64)
    );
    assert_eq!(
        evaluate_source("9223372036854775807 - -1", &context),
        json!(9223372036854775808u64)
    );
    assert_eq!(
        evaluate_source("-(-9223372036854775808)", &context),
        json!(9223372036854775808u64)
    );
    assert_eq!(
        evaluate_source("-9223372036854775808 % -1", &context),
        json!(0)
    );
    for source in [
        "18446744073709551615 + 1",
        "9223372036854775807 * 3",
        "-9223372036854775808 - 1",
        "1 / 0",
        "1 % 0",
    ] {
        assert!(evaluate(&expression(source), &context, &EvaluationLimits::default()).is_err());
    }
    for source in ["'1' + 2", "true + true"] {
        assert!(compile_expression(source, &CompilationLimits::default()).is_err());
    }
}

#[test]
fn large_integer_comparison_does_not_round_through_f64() {
    let context = json!({});
    assert_eq!(
        evaluate_source("9007199254740993 == 9007199254740992.0", &context),
        json!(false)
    );
    assert_eq!(
        evaluate_source("9007199254740993 > 9007199254740992.0", &context),
        json!(true)
    );
    assert_eq!(
        evaluate_source("9223372036854775807 < 9223372036854775808.0", &context),
        json!(true)
    );
    assert_eq!(
        evaluate_source("-9223372036854775808 == -9223372036854775808.0", &context),
        json!(true)
    );
    assert_eq!(
        evaluate_source("18446744073709551615 < 18446744073709551616.0", &context),
        json!(true)
    );
    assert_eq!(evaluate_source("-0.0 == 0", &context), json!(true));
}

#[test]
fn equality_is_deep_and_ordering_rejects_unsupported_types() {
    let context = json!({});
    assert_eq!(
        evaluate_source("{a: [1, {b: true}]} == {a: [1, {b: true}]}", &context),
        json!(true)
    );
    assert_eq!(evaluate_source("'z' > 'a'", &context), json!(true));
    assert!(compile_expression("[] < []", &CompilationLimits::default()).is_err());
}

#[test]
fn boolean_operators_are_strict_and_short_circuit() {
    let context = json!({});
    assert_eq!(
        evaluate_source("false && missing.value", &context),
        json!(false)
    );
    assert_eq!(
        evaluate_source("true || missing.value", &context),
        json!(true)
    );
    assert_eq!(evaluate_source("!false", &context), json!(true));
    assert!(evaluate_bool(&expression("1"), &context, &EvaluationLimits::default()).is_err());
    assert!(compile_expression("1 && true", &CompilationLimits::default()).is_err());
}

#[test]
fn parser_rejects_ambiguous_nullish_boolean_mix() {
    for source in ["state.a ?? false || true", "true && state.a ?? false"] {
        let error = compile_expression(source, &CompilationLimits::default()).unwrap_err();
        assert_eq!(error.phase(), ErrorPhase::Parse);
        assert!(error.message().contains("cannot be mixed"));
    }
    assert!(
        compile_expression("(state.a ?? false) || true", &CompilationLimits::default()).is_ok()
    );
}

#[test]
fn operators_are_left_associative_and_ternaries_are_right_associative() {
    let context = json!({});
    assert_eq!(evaluate_source("20 - 5 - 3", &context), json!(12));
    assert_eq!(evaluate_source("20 / 5 / 2", &context), json!(2.0));
    assert_eq!(
        evaluate_source("false ? 1 : true ? 2 : 3", &context),
        json!(2)
    );
}

#[test]
fn missing_null_and_falsy_values_have_distinct_coalescing_semantics() {
    let context =
        json!({"state": {"null": null, "false": false, "zero": 0, "empty": "", "list": []}});
    assert_eq!(
        evaluate_source("state.absent.child ?? 4", &context),
        json!(4)
    );
    assert_eq!(evaluate_source("state.null.child ?? 5", &context), json!(5));
    assert_eq!(
        evaluate_source("state.false ?? true", &context),
        json!(false)
    );
    assert_eq!(evaluate_source("state.zero ?? 8", &context), json!(0));
    assert_eq!(evaluate_source("state.empty ?? 'x'", &context), json!(""));
    assert_eq!(evaluate_source("state.list ?? [1]", &context), json!([]));
    assert!(evaluate(
        &expression("state.absent"),
        &context,
        &EvaluationLimits::default()
    )
    .is_err());
}

#[test]
fn indexing_rules_are_loud_except_for_absent_elements() {
    let context = json!({"items": ["first"], "object": {"key": 7}, "scalar": 4});
    assert_eq!(
        evaluate_source("items[10] ?? 'fallback'", &context),
        json!("fallback")
    );
    assert_eq!(evaluate_source("object['key']", &context), json!(7));
    for source in [
        "items[-1]",
        "items[0.5]",
        "items['0']",
        "object[0]",
        "scalar.x",
    ] {
        assert!(evaluate(&expression(source), &context, &EvaluationLimits::default()).is_err());
    }
}

#[test]
fn exists_only_suppresses_missing_paths() {
    let context = json!({"state": {"present": null, "items": [1], "scalar": 1}});
    assert_eq!(
        evaluate_source("exists(state.present)", &context),
        json!(true)
    );
    assert_eq!(
        evaluate_source("exists(state.absent.child)", &context),
        json!(false)
    );
    assert_eq!(
        evaluate_source("exists(state.items[3])", &context),
        json!(false)
    );
    assert!(compile_expression("exists(1 + 2)", &CompilationLimits::default()).is_err());
    assert!(evaluate(
        &expression("exists(state.scalar.child)"),
        &context,
        &EvaluationLimits::default()
    )
    .is_err());
}

#[test]
fn dynamic_indices_must_succeed_even_when_the_target_is_missing() {
    let context = json!({"state": {}});
    assert_eq!(
        evaluate_source("state.absent['key'] ?? 4", &context),
        json!(4)
    );
    assert!(evaluate(
        &expression("state.absent[state.missing] ?? 4"),
        &context,
        &EvaluationLimits::default()
    )
    .is_err());
    assert!(evaluate(
        &expression("state.absent[1 / 0] ?? 4"),
        &context,
        &EvaluationLimits::default()
    )
    .is_err());
    for source in [
        "state.absent[true] ?? 4",
        "state.absent[-1] ?? 4",
        "state.absent[0.5] ?? 4",
        "exists(state.absent[null])",
    ] {
        assert!(
            evaluate(&expression(source), &context, &EvaluationLimits::default()).is_err(),
            "invalid dynamic index must remain loud for {source}"
        );
    }
}

#[test]
fn membership_and_contains_cover_arrays_objects_and_strings() {
    let context = json!({});
    assert_eq!(evaluate_source("{a: 1} in [{a: 1}]", &context), json!(true));
    assert_eq!(evaluate_source("'a' in {a: 1}", &context), json!(true));
    assert_eq!(evaluate_source("'ell' in 'hello'", &context), json!(true));
    assert_eq!(
        evaluate_source("contains([1, 2], 2)", &context),
        json!(true)
    );
    assert!(compile_expression("1 in '123'", &CompilationLimits::default()).is_err());
}

#[test]
fn collection_and_string_functions_are_deterministic() {
    let context = json!({});
    assert_eq!(evaluate_source("length('a😀')", &context), json!(2));
    assert_eq!(evaluate_source("length([1, 2, 3])", &context), json!(3));
    assert_eq!(evaluate_source("length({a: 1, b: 2})", &context), json!(2));
    assert_eq!(
        evaluate_source("keys({z: 1, a: 2})", &context),
        json!(["a", "z"])
    );
    assert_eq!(
        evaluate_source("upper('Straße')", &context),
        json!("STRASSE")
    );
    assert_eq!(evaluate_source("lower('İ')", &context), json!("i̇"));
    assert_eq!(evaluate_source("type([1])", &context), json!("array"));
}

#[test]
fn json_and_string_have_distinct_string_semantics() {
    let context = json!({});
    assert_eq!(evaluate_source("json('hi')", &context), json!("\"hi\""));
    assert_eq!(evaluate_source("string('hi')", &context), json!("hi"));
    assert_eq!(
        evaluate_source("json({z: 1, a: {y: 2, x: 3}})", &context),
        json!(r#"{"a":{"x":3,"y":2},"z":1}"#)
    );
    assert_eq!(
        evaluate_source("string({z: 1, a: 2})", &context),
        json!(r#"{"a":2,"z":1}"#)
    );
    assert_eq!(evaluate_source("string(null)", &context), json!("null"));
}

#[test]
fn from_json_and_number_require_complete_canonical_input() {
    let context = json!({});
    assert_eq!(
        evaluate_source(r#"from_json('{"a":[1,true]}')"#, &context),
        json!({"a": [1, true]})
    );
    assert_eq!(evaluate_source("number('42')", &context), json!(42));
    assert_eq!(evaluate_source("number('-1.5e2')", &context), json!(-150.0));
    assert_eq!(evaluate_source("number(7)", &context), json!(7));
    for source in [
        r#"from_json('1 2')"#,
        "number(' 1')",
        "number('01')",
        "number('1x')",
    ] {
        assert!(evaluate(&expression(source), &context, &EvaluationLimits::default()).is_err());
    }
}

#[test]
fn regex_matching_is_bounded_and_reports_invalid_patterns() {
    let context = json!({});
    assert_eq!(
        evaluate_source(r#"matches('abc123', '^[a-z]+\\d+$')"#, &context),
        json!(true)
    );
    assert!(evaluate(
        &expression("matches('x', '[')"),
        &context,
        &EvaluationLimits::default()
    )
    .is_err());
    let limits = EvaluationLimits {
        max_regex_pattern_bytes: 2,
        ..EvaluationLimits::default()
    };
    let error = evaluate(&expression("matches('abc', 'abc')"), &context, &limits).unwrap_err();
    assert_eq!(error.phase(), ErrorPhase::Limit);
}

#[test]
fn whole_expression_templates_preserve_native_json() {
    let context = json!({"state": {"count": 2, "items": [1, 2]}});
    assert_eq!(render("${state.count + 1}", &context), json!(3));
    assert_eq!(render("${state.items}", &context), json!([1, 2]));
    assert_eq!(render("literal", &context), json!("literal"));
}

#[test]
fn embedded_templates_are_scalar_and_one_pass() {
    let context = json!({"state": {"count": 2, "generated": "${state.count}"}});
    assert_eq!(
        render(
            "round ${state.count + 1}; enabled=${true}; none=${null}",
            &context
        ),
        json!("round 3; enabled=true; none=")
    );
    assert_eq!(render("$${state.count}", &context), json!("${state.count}"));
    assert_eq!(
        render("generated: ${state.generated}", &context),
        json!("generated: ${state.count}")
    );
    assert!(compile_and_render(
        "items=${state.items}",
        &context,
        &CompilationLimits::default(),
        &EvaluationLimits::default()
    )
    .is_err());
    assert_eq!(
        render("items=${json(state.items)}", &context),
        json!("items=[1,2]")
    );
}

#[test]
fn scanner_handles_nested_delimiters_and_quoted_braces() {
    let context = json!({});
    assert_eq!(
        render(
            r#"value=${json({text: "}", nested: [1, {x: "]"}]})}"#,
            &context
        ),
        json!(r#"value={"nested":[1,{"x":"]"}],"text":"}"}"#)
    );
    let error = compile_template("before ${ {a: 1] }", &CompilationLimits::default()).unwrap_err();
    assert_eq!(error.phase(), ErrorPhase::Scan);
}

#[test]
fn scanner_respects_escaped_quotes_and_escape_prefix_boundaries() {
    let context = json!({});
    assert_eq!(
        render(r#"${"quoted \"} brace }"}"#, &context),
        json!(r#"quoted "} brace }"#)
    );
    assert_eq!(render("$$${1}", &context), json!("$${1}"));
    assert_eq!(render("$${1}${2}", &context), json!("${1}2"));
}

#[test]
fn template_compiler_rejects_removed_input_shorthand_in_literal_text() {
    let error =
        compile_template("Question: {input:question}", &CompilationLimits::default()).unwrap_err();

    assert_eq!(error.phase(), ErrorPhase::Parse);
    assert!(error.to_string().contains("removed `{input:...}`"));
    // Text inside an expression string is data, not a legacy reference.
    assert_eq!(
        render(r#"${"literal {input:question}"}"#, &json!({})),
        json!("literal {input:question}")
    );
}

#[test]
fn lexer_enforces_json_numbers_and_unicode_escapes() {
    let context = json!({});
    assert_eq!(evaluate_source(r#""\uD83D\uDE00""#, &context), json!("😀"));
    assert_eq!(evaluate_source(r#"'it\'s'"#, &context), json!("it's"));
    for source in ["01", "1.", "1e", r#""\uD800""#, r#""\q""#] {
        assert!(compile_expression(source, &CompilationLimits::default()).is_err());
    }
}

#[test]
fn compiler_rejects_unknown_functions_arity_and_duplicate_keys() {
    for source in [
        "len([])",
        "length()",
        "contains([1])",
        "{a: 1, a: 2}",
        "[1,]",
    ] {
        assert!(compile_expression(source, &CompilationLimits::default()).is_err());
    }
}

#[test]
fn compiler_rejects_statically_impossible_operands() {
    let limits = CompilationLimits::default();
    for source in [
        "!1",
        "+false",
        "1 && true",
        "false || 'fallback'",
        "'x' - 1",
        "'x' + 1",
        "true < inputs.value",
        "1 < '2'",
        "1 in {key: true}",
        "length(1)",
        "contains('abc', 1)",
        "keys([])",
        "upper(false)",
        "from_json({})",
        "matches('value', 1)",
        "number(false)",
        "1 ? true : false",
    ] {
        let error = compile_expression_for(source, "test.expression", &limits).unwrap_err();
        assert_eq!(error.phase(), ErrorPhase::Parse, "{source}: {error}");
        assert!(error.message().contains("statically"), "{source}: {error}");
    }
}

#[test]
fn compiler_preserves_runtime_dependent_operand_types() {
    let limits = CompilationLimits::default();
    for source in [
        "!inputs.flag",
        "inputs.left && true",
        "inputs.left + 'suffix'",
        "inputs.left - 1",
        "inputs.left < 1",
        "length(inputs.value)",
        "contains(inputs.container, 1)",
        "inputs.condition ? 'yes' : 'no'",
    ] {
        compile_expression(source, &limits)
            .unwrap_or_else(|error| panic!("dynamic operand must compile for {source}: {error}"));
    }
}

#[test]
fn legacy_boolean_fallback_is_rejected_with_nullish_migration() {
    let limits = CompilationLimits::default();
    let error = compile_template_for(r#"${inputs.name || "default"}"#, "directive.body", &limits)
        .unwrap_err();
    assert_eq!(error.phase(), ErrorPhase::Parse);
    assert!(error.message().contains("expected bool"));
    assert!(error.correction_text().unwrap_or_default().contains("`??`"));

    compile_template_for("${inputs.ready || false}", "directive.body", &limits)
        .expect("boolean OR remains valid");
}

#[test]
fn single_pipe_reports_the_removed_filter_migration() {
    let error = compile_expression_for(
        "inputs.items | length",
        "graph.nodes.collect.assign.count",
        &CompilationLimits::default(),
    )
    .unwrap_err();
    assert_eq!(error.phase(), ErrorPhase::Lex);
    assert!(error.message().contains("pipe-filter syntax was removed"));
    assert!(error
        .correction_text()
        .unwrap_or_default()
        .contains("length(value)"));
}

#[test]
fn references_distinguish_exact_indices_and_dynamic_access() {
    let compiled = expression("inputs.name + inputs['other'] + items[0].id + records[inputs.key]");
    let references = compiled.references();
    assert!(references.contains_exact("inputs", &["name"]));
    assert!(references.contains_exact("inputs", &["other"]));
    let records = references
        .iter()
        .find(|reference| reference.root() == "records")
        .unwrap();
    assert_eq!(records.segments(), &[ReferenceSegment::Dynamic]);
    let items = references
        .iter()
        .find(|reference| reference.root() == "items")
        .unwrap();
    assert_eq!(
        items.segments(),
        &[
            ReferenceSegment::Index(0),
            ReferenceSegment::Key("id".to_string())
        ]
    );
    assert!(references.roots().any(|root| root == "inputs"));
}

#[test]
fn grouped_literal_indices_remain_static_references() {
    let compiled = expression("inputs[((\"name\"))] + items[((0))] + records[(inputs.key)]");
    assert!(compiled.references().contains_exact("inputs", &["name"]));
    let items = compiled
        .references()
        .iter()
        .find(|reference| reference.root() == "items")
        .unwrap();
    assert_eq!(items.segments(), &[ReferenceSegment::Index(0)]);
    let records = compiled
        .references()
        .iter()
        .find(|reference| reference.root() == "records")
        .unwrap();
    assert_eq!(records.segments(), &[ReferenceSegment::Dynamic]);
    assert!(compiled.references().contains_exact("inputs", &["key"]));
}

#[test]
fn reference_sets_can_be_aggregated_publicly() {
    let mut references = expression("inputs.first").references().clone();
    references.extend(expression("state.second").references());
    assert!(references.contains_exact("inputs", &["first"]));
    assert!(references.contains_exact("state", &["second"]));
}

#[test]
fn condition_normalization_accepts_only_scalar_expression_forms() {
    let limits = CompilationLimits::default();
    let bare = compile_condition_for(" state.ready ", "next.when", &limits).unwrap();
    let wrapped = compile_condition_for("${state.ready}", "next.when", &limits).unwrap();
    let literal_template_marker =
        compile_condition_for(r#"state.label == "${literal}""#, "next.when", &limits).unwrap();
    assert_eq!(bare.references(), wrapped.references());
    assert!(literal_template_marker
        .references()
        .contains_exact("state", &["label"]));
    assert!(compile_condition_for("", "next.when", &limits).is_err());
    assert!(compile_condition_for("prefix ${state.ready}", "next.when", &limits).is_err());
}

#[test]
fn condition_compiler_rejects_statically_non_boolean_expressions() {
    for source in ["1", "null", "[1]", "{value: true}", "'text'", "1 + 2"] {
        let error = compile_condition_for(
            source,
            "graph.nodes.gate.next.when",
            &CompilationLimits::default(),
        )
        .unwrap_err();

        assert!(
            error.to_string().contains("expected bool"),
            "{source}: {error}"
        );
    }
    compile_condition_for(
        "state.ready",
        "graph.nodes.gate.next.when",
        &CompilationLimits::default(),
    )
    .expect("runtime-dependent condition type remains valid until evaluation");
}

#[test]
fn diagnostics_retain_phase_field_and_unicode_line_column() {
    let error = compile_expression_for(
        "true &&\n😀",
        "graph.next.when",
        &CompilationLimits::default(),
    )
    .unwrap_err();
    assert_eq!(error.field(), Some("graph.next.when"));
    assert!(matches!(error.phase(), ErrorPhase::Lex | ErrorPhase::Parse));
    assert_eq!(error.line_column(), (2, 1));
    assert!(error.to_string().contains("graph.next.when"));
    assert!(error.to_string().contains("source \"😀\""));
}

#[test]
fn compilation_limits_cover_source_template_tokens_depth_and_literals() {
    let tiny_source = CompilationLimits {
        max_source_bytes: 2,
        ..CompilationLimits::default()
    };
    assert_eq!(
        compile_expression("true", &tiny_source)
            .unwrap_err()
            .phase(),
        ErrorPhase::Limit
    );

    let tiny_template = CompilationLimits {
        max_template_bytes: 2,
        ..CompilationLimits::default()
    };
    assert_eq!(
        compile_template("text", &tiny_template)
            .unwrap_err()
            .phase(),
        ErrorPhase::Limit
    );

    let one_expression = CompilationLimits {
        max_expressions_per_template: 1,
        ..CompilationLimits::default()
    };
    assert!(compile_template("${1}${2}", &one_expression).is_err());

    let shallow = CompilationLimits {
        max_ast_depth: 2,
        ..CompilationLimits::default()
    };
    assert!(compile_expression("(((1)))", &shallow).is_err());

    let one_literal = CompilationLimits {
        max_literal_elements: 1,
        ..CompilationLimits::default()
    };
    assert!(compile_expression("[1, 2]", &one_literal).is_err());

    let two_tokens = CompilationLimits {
        max_tokens: 2,
        ..CompilationLimits::default()
    };
    assert!(compile_expression("1 + 2", &two_tokens).is_err());

    let one_argument = CompilationLimits {
        max_function_arguments: 1,
        ..CompilationLimits::default()
    };
    assert!(compile_expression("contains([], 1)", &one_argument).is_err());
}

#[test]
fn evaluation_limits_are_cumulative_across_a_session() {
    let context = json!({});
    let limits = EvaluationLimits {
        max_allocation_bytes: 7,
        ..EvaluationLimits::default()
    };
    let compiled = expression("'abcd'");
    let mut session = EvaluationSession::new(&context, &limits);
    assert_eq!(session.evaluate(&compiled).unwrap(), json!("abcd"));
    assert!(session.evaluate(&compiled).is_err());
    assert_eq!(session.allocated_bytes(), 4);
}

#[test]
fn container_allocation_is_rejected_before_budget_commit() {
    let context = json!({});
    let limits = EvaluationLimits {
        max_allocation_bytes: 1,
        ..EvaluationLimits::default()
    };
    let mut session = EvaluationSession::new(&context, &limits);
    assert!(session.evaluate(&expression("{a: 1}")).is_err());
    assert_eq!(session.allocated_bytes(), 0);
    assert!(session.evaluate(&expression("[1]")).is_err());
    assert_eq!(session.allocated_bytes(), 0);
}

#[test]
fn serialized_result_bytes_consume_fuel() {
    let context = json!({});
    let limits = EvaluationLimits {
        fuel: 4,
        ..EvaluationLimits::default()
    };
    let error = evaluate(&expression("'x'"), &context, &limits).unwrap_err();
    assert_eq!(error.phase(), ErrorPhase::Limit);
}

#[test]
fn missing_path_materialization_is_allocation_bounded() {
    let context = json!({});
    let limits = EvaluationLimits {
        max_allocation_bytes: 3,
        ..EvaluationLimits::default()
    };
    let mut session = EvaluationSession::new(&context, &limits);
    let error = session.evaluate(&expression("missing ?? 1")).unwrap_err();
    assert_eq!(error.phase(), ErrorPhase::Limit);
    assert_eq!(session.allocated_bytes(), 0);
}

#[test]
fn evaluation_fuel_and_result_limits_fail_closed() {
    let context = json!({"state": {"value": "long"}});
    let fuel = EvaluationLimits {
        fuel: 1,
        ..EvaluationLimits::default()
    };
    assert_eq!(
        evaluate(&expression("state.value"), &context, &fuel)
            .unwrap_err()
            .phase(),
        ErrorPhase::Limit
    );
    let result = EvaluationLimits {
        max_result_bytes: 2,
        ..EvaluationLimits::default()
    };
    assert_eq!(
        evaluate(&expression("state.value"), &context, &result)
            .unwrap_err()
            .phase(),
        ErrorPhase::Limit
    );
}

#[test]
fn evaluator_enforces_each_data_dependent_limit() {
    let context = json!({"state": {"nested": [[1, 2]], "text": "long"}});

    let scalar = EvaluationLimits {
        max_scalar_bytes: 2,
        ..EvaluationLimits::default()
    };
    assert!(evaluate(&expression("state.text"), &context, &scalar).is_err());

    let produced = EvaluationLimits {
        max_produced_string_bytes: 2,
        ..EvaluationLimits::default()
    };
    assert!(render_template(
        &compile_template("x${'yz'}", &CompilationLimits::default()).unwrap(),
        &context,
        &produced
    )
    .is_err());
    for source in ["type(state.text)", "string(true)", "keys({long: 1})"] {
        let error = evaluate(&expression(source), &context, &produced).unwrap_err();
        assert_eq!(error.phase(), ErrorPhase::Limit, "source: {source}");
    }

    let elements = EvaluationLimits {
        max_container_elements: 1,
        ..EvaluationLimits::default()
    };
    assert!(evaluate(&expression("state.nested"), &context, &elements).is_err());

    let traversal_depth = EvaluationLimits {
        max_traversal_depth: 1,
        ..EvaluationLimits::default()
    };
    assert!(evaluate(&expression("state.nested"), &context, &traversal_depth).is_err());

    let result_depth = EvaluationLimits {
        max_result_depth: 1,
        ..EvaluationLimits::default()
    };
    assert!(evaluate(&expression("[1]"), &context, &result_depth).is_err());

    let result_nodes = EvaluationLimits {
        max_result_nodes: 1,
        ..EvaluationLimits::default()
    };
    assert!(evaluate(&expression("[1]"), &context, &result_nodes).is_err());

    let json_input = EvaluationLimits {
        max_from_json_bytes: 2,
        ..EvaluationLimits::default()
    };
    assert!(evaluate(&expression("from_json('[1]')"), &context, &json_input).is_err());

    let regex_haystack = EvaluationLimits {
        max_regex_haystack_bytes: 2,
        ..EvaluationLimits::default()
    };
    assert!(evaluate(
        &expression("matches('long', '.*')"),
        &context,
        &regex_haystack
    )
    .is_err());
}

#[test]
fn borrowed_root_lookup_does_not_clone_unselected_subtrees() {
    let context = json!({
        "state": {
            "selected": 7,
            "unrelated": (0..1000).collect::<Vec<_>>()
        }
    });
    let limits = EvaluationLimits {
        max_container_elements: 1,
        max_allocation_bytes: 1,
        ..EvaluationLimits::default()
    };
    assert_eq!(
        evaluate(&expression("state.selected"), &context, &limits).unwrap(),
        json!(7)
    );
}

#[test]
fn borrowed_root_context_avoids_building_a_combined_json_object() {
    let state = json!({"count": 3});
    let inputs = json!({"increment": 2});
    let context = EvaluationContext::new()
        .with_root("state", &state)
        .with_root("inputs", &inputs);
    assert_eq!(context.roots().count(), 2);
    let limits = EvaluationLimits::default();
    let mut session = EvaluationSession::with_context(&context, &limits);
    assert_eq!(
        session
            .evaluate(&expression("state.count + inputs.increment"))
            .unwrap(),
        json!(5)
    );
}

#[test]
fn explicit_session_charges_share_the_evaluation_budget() {
    let context = json!({});
    let limits = EvaluationLimits {
        max_container_elements: 2,
        max_allocation_bytes: 4,
        ..EvaluationLimits::default()
    };
    let mut session = EvaluationSession::new(&context, &limits);
    session.charge_allocation(4, "assign").unwrap();
    assert!(session.charge_allocation(1, "assign").is_err());
    session.charge_container_elements(2, "foreach").unwrap();
    assert!(session.charge_container_elements(1, "foreach").is_err());
}

#[test]
fn assembled_result_shape_is_checked_against_shared_limits() {
    let context = json!({});
    let limits = EvaluationLimits {
        max_result_depth: 2,
        max_result_nodes: 3,
        max_result_bytes: 8,
        ..EvaluationLimits::default()
    };
    let mut session = EvaluationSession::new(&context, &limits);
    session.charge_result_shape(2, 3, 8, "params").unwrap();
    assert!(session.charge_result_shape(3, 3, 8, "params").is_err());
    assert!(session.charge_result_shape(2, 4, 8, "params").is_err());
    assert!(session.charge_result_shape(2, 3, 9, "params").is_err());
}

#[test]
fn clone_value_is_bounded_and_field_aware() {
    let context = json!({});
    let limits = EvaluationLimits {
        max_scalar_bytes: 2,
        ..EvaluationLimits::default()
    };
    let mut session = EvaluationSession::new(&context, &limits);
    let error = session
        .clone_value(&json!({"value": "long"}), "assign.delta")
        .unwrap_err();
    assert_eq!(error.phase(), ErrorPhase::Limit);
    assert_eq!(error.field(), Some("assign.delta"));
}
