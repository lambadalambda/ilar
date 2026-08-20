use ilar::command::{CommandStore, expand};

fn write(path: &std::path::Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn store(user: &std::path::Path, project: &std::path::Path) -> CommandStore {
    CommandStore::new(user.into(), project.into())
}

#[test]
fn arguments_substitute_whole_and_positionally() {
    assert_eq!(
        expand("Fix $ARGUMENTS now", "the parser"),
        "Fix the parser now"
    );
    assert_eq!(expand("$1 then $2", "alpha beta"), "alpha then beta");
    assert_eq!(expand("$1 and $2", "only"), "only and ");
    assert_eq!(expand("Nothing: $ARGUMENTS", ""), "Nothing: ");
    // Quoted groups stay together.
    assert_eq!(expand("$1|$2", "\"two words\" second"), "two words|second");
}

/// `$` is common in command bodies — shell snippets especially — so
/// only the placeholder forms are touched. A `$` followed by a digit is
/// always a placeholder, though, so an unmatched one empties rather than
/// staying literal: consistency beats guessing which `$5` was meant.
#[test]
fn only_placeholder_forms_are_substituted() {
    assert_eq!(expand("shell $(date)", "x"), "shell $(date)");
    assert_eq!(expand("${HOME}/bin", "x"), "${HOME}/bin");
    assert_eq!(expand("costs $$ and $NAME", "x"), "costs $$ and $NAME");
    assert_eq!(expand("trailing $", "x"), "trailing $");
    // `$10` is `$1` then a literal zero, as in a shell.
    assert_eq!(expand("$10 is one then zero", "a"), "a0 is one then zero");
    // Unmatched positional empties.
    assert_eq!(expand("costs $5", "one"), "costs ");
}

#[test]
fn commands_load_from_both_roots_with_project_winning() {
    let user = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    write(
        &user.path().join("commands/review.md"),
        "---\ndescription: Review a PR\n---\nReview $ARGUMENTS carefully.\n",
    );
    write(
        &user.path().join("commands/only-user.md"),
        "---\ndescription: User only\n---\nBody\n",
    );
    write(
        &project.path().join(".ilar/commands/review.md"),
        "---\ndescription: Project review\n---\nProject body\n",
    );

    let commands = store(user.path(), project.path()).list().unwrap();
    let review = commands.iter().find(|c| c.name == "review").unwrap();
    assert_eq!(review.description, "Project review");
    // Body kept verbatim apart from the newline after the delimiter:
    // an indented code block must survive intact.
    assert_eq!(review.template, "Project body\n");
    assert!(commands.iter().any(|c| c.name == "only-user"));
}

#[test]
fn a_command_without_a_body_is_rejected_by_name() {
    let user = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    write(
        &user.path().join("commands/empty.md"),
        "---\ndescription: Nothing here\n---\n\n",
    );
    let error = store(user.path(), project.path()).list().unwrap_err();
    let rendered = format!("{error:#}");
    assert!(rendered.contains("empty"), "{rendered}");
    assert!(rendered.contains("body"), "{rendered}");
}

/// The frontmatter parser is shared with skills, so an opencode command
/// file works untouched.
#[test]
fn yaml_command_files_load_unchanged() {
    let user = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    write(
        &user.path().join("commands/greptile.md"),
        "---\ndescription: Address Greptile PR comments\nagent: build\n---\nAddress Greptile feedback.\n\nCommand arguments: $ARGUMENTS\n",
    );

    let commands = store(user.path(), project.path()).list().unwrap();
    let greptile = commands.iter().find(|c| c.name == "greptile").unwrap();
    assert_eq!(greptile.description, "Address Greptile PR comments");
    assert!(greptile.template.contains("Command arguments: $ARGUMENTS"));
    // Optional frontmatter is kept even though nothing honours it yet.
    assert_eq!(greptile.agent.as_deref(), Some("build"));
}

#[test]
fn apostrophes_and_quotes_split_the_way_a_reader_expects() {
    // An apostrophe mid-word is an apostrophe, not an unterminated
    // quote that swallows every later argument.
    assert_eq!(expand("[$1][$2][$3]", "don't fix it"), "[don't][fix][it]");
    assert_eq!(
        expand("[$1][$2]", "\"two words\" second"),
        "[two words][second]"
    );
    assert_eq!(
        expand("[$1][$2]", "'single quoted' next"),
        "[single quoted][next]"
    );
    // An explicit empty argument holds its position.
    assert_eq!(expand("[$1][$2]", "\"\" second"), "[][second]");
}

#[test]
fn arguments_needs_a_word_boundary() {
    assert_eq!(expand("$ARGUMENTSX", "a"), "$ARGUMENTSX");
    assert_eq!(expand("$ARGUMENTS_LIST", "a"), "$ARGUMENTS_LIST");
    assert_eq!(expand("$ARGUMENTS.", "a"), "a.");
    assert_eq!(expand("$ARGUMENTS", "a"), "a");
}

#[test]
fn a_name_the_slash_syntax_cannot_express_is_rejected() {
    let user = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    write(
        &user.path().join("commands/my.command.md"),
        "---\ndescription: Dotted\n---\nBody\n",
    );
    let error = store(user.path(), project.path()).list().unwrap_err();
    assert!(
        format!("{error:#}").contains("cannot be invoked"),
        "{error:#}"
    );
}

#[test]
fn the_goal_name_is_reserved() {
    let user = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    write(
        &user.path().join("commands/goal.md"),
        "---\ndescription: Shadows the built-in\n---\nBody\n",
    );
    let error = store(user.path(), project.path()).list().unwrap_err();
    assert!(format!("{error:#}").contains("reserved"), "{error:#}");
}
