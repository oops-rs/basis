//! What the model is offered, what the approver is shown, and what comes back.
//!
//! The subprocess cases are gated to unix for [`crate::subprocess`]'s reason:
//! they run `/bin/sh` scripts, which is the cheapest way to exercise a real
//! program, and inventing a Windows equivalent per case would test the fixture
//! rather than the wrapper. Everything above them — the descriptor, the
//! preview, the input check — is portable and runs everywhere.

use serde_json::json;

use super::*;
use crate::tools::declared::manifest::SideEffect;

fn spec(command: Vec<&str>) -> DeclaredToolSpec {
    DeclaredToolSpec {
        name: "jenkins_job".to_string(),
        description: "Trigger a job and return its build number.".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {"job": {"type": "string"}},
            "required": ["job"],
        }),
        command: command.into_iter().map(str::to_string).collect(),
        cwd: None,
        env: Vec::new(),
        timeout_ms: None,
        side_effect: SideEffect::Process,
    }
}

fn tool(command: Vec<&str>) -> DeclaredTool {
    DeclaredTool::new(spec(command), "/repo")
}

#[test]
fn the_model_is_offered_the_declaration_as_written() {
    let descriptor = tool(vec!["./ci/jenkins"]).descriptor();

    assert_eq!(descriptor.provider.name, "jenkins_job");
    assert_eq!(
        descriptor.provider.description.as_deref(),
        Some("Trigger a job and return its build number.")
    );
    assert_eq!(
        descriptor.provider.input_schema.get("required"),
        Some(&json!(["job"])),
        "the schema the manifest wrote is the schema the model fills in"
    );
}

#[test]
fn a_declared_tool_is_never_read_only_and_never_batched() {
    let descriptor = tool(vec!["./ci/jenkins"]).descriptor();

    assert!(
        crate::approval::is_consequential(descriptor.side_effect_level),
        "a program that runs is something to be asked about"
    );
    assert!(
        !descriptor.execution_category.allows_parallel(),
        "basis cannot know what somebody's program writes"
    );
}

#[test]
fn mentras_deadline_is_set_behind_the_one_that_kills_the_process() {
    // Ours stops the program; mentra's only abandons the wait. If theirs fired
    // first the model would read a message that names no tool and stop nothing.
    let declared = spec(vec!["./ci/jenkins"]);
    let descriptor = DeclaredTool::new(declared.clone(), "/repo").descriptor();

    assert!(
        descriptor.execution_timeout.expect("a backstop") > declared.timeout(),
        "the tool's own message has to be the one that arrives"
    );
}

#[test]
fn the_approver_is_shown_the_program_that_is_about_to_run() {
    // The name is chosen by the same file that chooses the command, so the name
    // is not evidence: an approver seeing only `jenkins_job` is approving a
    // string a repository wrote.
    let tool = tool(vec!["./ci/jenkins", "--trigger"]);
    let input = json!({"job": "nightly"});

    let preview = preview(tool.spec(), Path::new("/repo"), &tool.descriptor(), &input)
        .expect("the call is well formed");

    assert_eq!(
        preview.structured_input,
        json!({
            "tool": "jenkins_job",
            "command": ["./ci/jenkins", "--trigger"],
            "cwd": "/repo",
            "input": {"job": "nightly"},
        })
    );
    assert_eq!(preview.working_directory, PathBuf::from("/repo"));
    assert_eq!(preview.raw_input, input);
}

#[test]
fn no_credential_reaches_the_approver_or_the_rule_it_writes() {
    // A preview travels further than a glance: it is globbed against remembered
    // rules and kept in the audit trail. The command is how a spawn is
    // understood; the environment is where the token is.
    let declared = DeclaredToolSpec {
        env: vec![("CI_TOKEN".to_string(), "secret-value".to_string())],
        ..spec(vec!["./ci/jenkins"])
    };
    let tool = DeclaredTool::new(declared, "/repo");

    let preview = preview(
        tool.spec(),
        Path::new("/repo"),
        &tool.descriptor(),
        &json!({"job": "nightly"}),
    )
    .expect("well formed");

    let rendered = preview.structured_input.to_string();
    assert!(!rendered.contains("secret-value"), "{rendered}");
    assert!(!rendered.contains("CI_TOKEN"), "{rendered}");
}

#[test]
fn a_schema_describing_nothing_still_gets_an_object() {
    // Upstream's rule reads a schema's object intent from `type`,
    // `properties` or `required`, and a schema declaring none of them is
    // deliberately left alone — correct for mentra, wrong for a binding that
    // writes the input to a program's stdin. `check_schema` accepts exactly
    // this manifest, so the residue is reachable and this is where it stops.
    for schema in [json!({}), json!({"description": "run the nightly job"})] {
        let loose = DeclaredToolSpec {
            input_schema: schema.clone(),
            ..spec(vec!["./x"])
        };

        let refused = check_input(&loose, &json!("nightly")).expect_err("not an object");
        assert!(refused.contains("jenkins_job"), "{refused} ({schema})");

        check_input(&loose, &json!({"job": "nightly"})).expect("an object always fits");
    }
}

#[test]
fn a_schema_upstream_can_read_is_left_to_upstream() {
    // The complement is strict: anything `root_shape_error` or the type check
    // can see is refused before this binding is reached, so re-refusing it
    // here would be two validators wording one refusal. Each of the three
    // keys upstream reads is enough to hand the call back.
    for schema in [
        json!({"type": "object", "properties": {}}),
        json!({"properties": {"job": {"type": "string"}}}),
        json!({"required": ["job"]}),
    ] {
        let declared = DeclaredToolSpec {
            input_schema: schema.clone(),
            ..spec(vec!["./x"])
        };

        check_input(&declared, &json!("nightly"))
            .unwrap_or_else(|error| panic!("upstream's to refuse, not ours: {error} ({schema})"));
    }
}

#[test]
fn a_call_that_fits_is_not_second_guessed_here() {
    // `required` is mentra's, checked before authorization and with the
    // missing field named. What is left here says nothing about a call that
    // is an object — including the empty one, which a schema requiring
    // nothing accepts.
    let declared = DeclaredToolSpec {
        input_schema: json!({"type": "object", "properties": {}}),
        ..spec(vec!["./x"])
    };

    check_input(&declared, &json!({})).expect("nothing is required");
    check_input(&spec(vec!["./x"]), &json!({"job": "nightly"})).expect("a call that fits");
}

#[test]
fn the_manifest_wins_over_the_runtime_for_the_same_name() {
    // The precedence rule, as a function, because it is the whole of the
    // design decision: the runtime's pairs are the host's statement about every
    // process it spawns, and the manifest's are this one tool's own — the more
    // specific statement about a name is the one that holds.
    let runtime = [
        ("NOUS_URL".to_string(), "http://nous".to_string()),
        ("SHARED".to_string(), "runtime".to_string()),
    ];
    let manifest = [("SHARED".to_string(), "manifest".to_string())];

    assert_eq!(
        environment(&runtime, &manifest),
        vec![
            ("NOUS_URL".to_string(), "http://nous".to_string()),
            ("SHARED".to_string(), "manifest".to_string()),
        ],
        "the overridden name appears once, with the manifest's value"
    );
}

#[test]
fn either_side_alone_is_simply_that_side() {
    let runtime = [("NOUS_URL".to_string(), "http://nous".to_string())];
    let manifest = [("CI_TOKEN".to_string(), "secret".to_string())];

    assert_eq!(environment(&runtime, &[]), runtime.to_vec());
    assert_eq!(environment(&[], &manifest), manifest.to_vec());
    assert!(environment(&[], &[]).is_empty(), "and nothing is nothing");
}

#[test]
fn a_declared_tool_carries_no_runtime_environment_until_it_is_given_one() {
    // `new` takes what the manifest said; the runtime's contribution arrives
    // separately, from the workspace open that has a runtime in hand.
    let bare = tool(vec!["./ci/jenkins"]);
    assert!(bare.command_environment().is_empty());

    let given =
        bare.with_command_environment([("NOUS_URL".to_string(), "http://nous".to_string())]);
    assert_eq!(
        given.command_environment(),
        [("NOUS_URL".to_string(), "http://nous".to_string())]
    );
}

#[test]
fn neither_environment_is_printed() {
    // `Debug` is hand-written on both this and the spec for one reason, and a
    // second field carrying host-supplied values is exactly why it stays that
    // way.
    let printed = format!(
        "{:?}",
        DeclaredTool::new(
            DeclaredToolSpec {
                env: vec![("CI_TOKEN".to_string(), "manifest-secret".to_string())],
                ..spec(vec!["./ci/jenkins"])
            },
            "/repo",
        )
        .with_command_environment([("NOUS_URL".to_string(), "runtime-secret".to_string())])
    );

    assert!(!printed.contains("manifest-secret"), "{printed}");
    assert!(!printed.contains("runtime-secret"), "{printed}");
    assert!(printed.contains("CI_TOKEN"), "a name is fixable: {printed}");
    assert!(printed.contains("NOUS_URL"), "{printed}");
}

#[cfg(unix)]
mod subprocess_cases {
    use super::*;

    fn sh(script: &str) -> Vec<&str> {
        vec!["/bin/sh", "-c", script]
    }

    /// Runs `script` as the tool's program, in a directory that exists, with
    /// the runtime contributing nothing — which is every host that never
    /// called `with_command_environment`.
    async fn call(script: &str, input: Value) -> ToolResult {
        run(Arc::new(spec(sh(script))), PathBuf::from("."), &[], input).await
    }

    /// The same, with the runtime's fixed pairs in force.
    async fn call_with_runtime_environment(
        declared: DeclaredToolSpec,
        runtime: &[(String, String)],
    ) -> ToolResult {
        run(
            Arc::new(declared),
            PathBuf::from("."),
            runtime,
            json!({"job": "nightly"}),
        )
        .await
    }

    fn pairs(named: &[(&str, &str)]) -> Vec<(String, String)> {
        named
            .iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
            .collect()
    }

    #[tokio::test]
    async fn the_call_arrives_as_json_on_stdin_and_stdout_comes_back() {
        // The whole binding in one assertion: the model filled a schema, the
        // program read an object, and nothing was quoted, escaped, or handed to
        // a shell in between.
        let answer = call("cat", json!({"job": "nightly build; rm -rf /"}))
            .await
            .expect("the program succeeded");

        assert_eq!(answer, r#"{"job":"nightly build; rm -rf /"}"#);
    }

    #[tokio::test]
    async fn a_value_a_shell_would_have_mangled_arrives_intact() {
        // The use case this shipped against: a query travelled base64-encoded
        // inside a command line to survive quoting. On stdin it does not have
        // to.
        let query = "select * from t where name = \"o'brien\" && $x > `date`\nsecond line";

        let answer = call("cat", json!({"job": "report", "query": query}))
            .await
            .expect("the program succeeded");

        let parsed: Value = serde_json::from_str(&answer).expect("valid JSON came back");
        assert_eq!(parsed["query"], json!(query));
    }

    #[tokio::test]
    async fn a_declared_variable_reaches_the_program_and_nothing_else_does() {
        let declared = DeclaredToolSpec {
            env: vec![("CI_TOKEN".to_string(), "secret-value".to_string())],
            ..spec(sh("printf %s \"$CI_TOKEN\""))
        };

        let answer = call_with_runtime_environment(declared, &[])
            .await
            .expect("the program succeeded");

        assert_eq!(answer, "secret-value");
    }

    #[tokio::test]
    async fn the_runtimes_command_environment_reaches_a_declared_program() {
        // The bug this fixes: a host called `with_command_environment` to say
        // where its service lives, commands through `spawn` were told, and a
        // declared tool's program was not — so it failed at the far end
        // complaining about a variable the runtime had been given.
        let answer =
            call_with_runtime_environment(spec(sh("printf %s \"$X\"")), &pairs(&[("X", "1")]))
                .await
                .expect("the program succeeded");

        assert_eq!(answer, "1");
    }

    #[tokio::test]
    async fn a_manifest_variable_overrides_the_runtimes_value_for_that_name() {
        let declared = DeclaredToolSpec {
            env: pairs(&[("X", "2")]),
            ..spec(sh("printf %s \"$X\""))
        };

        let answer = call_with_runtime_environment(declared, &pairs(&[("X", "1")]))
            .await
            .expect("the program succeeded");

        assert_eq!(answer, "2", "the tool's own statement is the specific one");
    }

    #[tokio::test]
    async fn path_arrives_from_the_baseline_and_a_parent_variable_does_not() {
        // The child's environment is what basis passed and nothing else: the
        // baseline (`PATH` is how the shell in every one of these cases is
        // found at all), the runtime's pairs, and the manifest's. Whatever
        // this process happens to be holding — a token, a proxy setting —
        // stops at the spawn. `/usr/bin/env` adds nothing of its own, so every
        // name it prints was either passed or leaked.
        let answer = call_with_runtime_environment(
            spec(vec!["/usr/bin/env"]),
            &pairs(&[("BASIS_TEST_RUNTIME", "1")]),
        )
        .await
        .expect("the program succeeded");

        let listed: Vec<&str> = answer
            .lines()
            .filter_map(|line| line.split_once('='))
            .map(|(name, _)| name)
            .collect();
        assert!(
            listed.contains(&"PATH"),
            "the baseline carries PATH: {answer}"
        );
        assert!(listed.contains(&"BASIS_TEST_RUNTIME"), "{answer}");
        for name in listed {
            assert!(
                name == "BASIS_TEST_RUNTIME" || subprocess::is_baseline(name),
                "`{name}` leaked from this process into the program: {answer}"
            );
        }
    }

    #[tokio::test]
    async fn a_failure_reaches_the_model_with_the_programs_own_words() {
        // A denial the model cannot act on is one it retries verbatim.
        let error = call("echo 'no such job' >&2; exit 4", json!({"job": "nope"}))
            .await
            .expect_err("exit 4");

        assert!(error.contains("jenkins_job"), "{error}");
        assert!(error.contains('4'), "{error}");
        assert!(error.contains("no such job"), "{error}");
    }

    #[tokio::test]
    async fn a_program_that_fails_on_stdout_is_still_quoted() {
        let error = call("echo 'no such job'; exit 1", json!({"job": "nope"}))
            .await
            .expect_err("exit 1");

        assert!(
            error.contains("no such job"),
            "plenty of programs never write stderr: {error}"
        );
    }

    #[tokio::test]
    async fn a_program_that_says_nothing_at_all_still_says_it_failed() {
        let error = call("exit 7", json!({"job": "nope"}))
            .await
            .expect_err("exit 7");

        assert!(error.contains("jenkins_job"), "{error}");
        assert!(error.contains('7'), "{error}");
    }

    #[tokio::test]
    async fn success_with_no_output_is_not_an_empty_result() {
        let answer = call("true", json!({"job": "nightly"}))
            .await
            .expect("exit 0");

        assert!(
            answer.contains("printed nothing"),
            "an empty result reads as a tool that did nothing: {answer}"
        );
    }

    #[tokio::test]
    async fn a_hanging_program_costs_the_turn_its_timeout_and_not_the_turn() {
        let declared = DeclaredToolSpec {
            timeout_ms: Some(150),
            ..spec(sh("sleep 30"))
        };
        let started = std::time::Instant::now();

        let error = call_with_runtime_environment(declared, &[])
            .await
            .expect_err("stopped at the deadline");

        assert!(error.contains("jenkins_job"), "{error}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the deadline, not the program, decides how long this takes"
        );
    }

    #[tokio::test]
    async fn a_program_that_is_not_there_is_a_tool_error_the_model_can_read() {
        let error =
            call_with_runtime_environment(spec(vec!["/definitely/not/a/real/program"]), &[])
                .await
                .expect_err("cannot be started");

        assert!(error.contains("jenkins_job"), "{error}");
        assert!(error.contains("could not be started"), "{error}");
    }
}
