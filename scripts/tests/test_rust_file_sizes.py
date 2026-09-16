"""Exercise Rust syntax counting and the actual Git-backed checker command."""

import json
import os
import runpy
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


CHECKER = Path(__file__).resolve().parents[1] / "check_rust_file_sizes.py"
COUNT = runpy.run_path(str(CHECKER))["count_source"]


class CountingTests(unittest.TestCase):
    def test_physical_lines_include_comments_blanks_and_final_line(self):
        for source, expected in [(b"", 0), (b"\n", 1), (b"// comment\n\nfn f() {}", 3), (b"// c\r\n\r\n", 2)]:
            with self.subTest(source=source):
                self.assertEqual(COUNT(source), (expected, 0, expected))

    def test_test_module_does_not_hide_later_production(self):
        source = b'''// production comment
#[cfg(test)]
mod tests {
    /* nested { /* } */ comment */
    fn check() { let s = r###" } { #[cfg(test)] "###; }
}

fn production() {}
'''
        self.assertEqual(COUNT(source), (3, 5, 8))

    def test_nested_test_items_and_attributes(self):
        source = b'''mod production {
    #[allow(dead_code)]
    #[cfg(test)]
    // belongs to the following item
    fn check() {}
    fn live() {}
}
'''
        self.assertEqual(COUNT(source), (3, 4, 7))

    def test_same_line_production_is_counted(self):
        for source in [b"#[cfg(test)] fn test() {} fn live() {}", b"fn live() {} #[cfg(test)] fn test() {}"]:
            self.assertEqual(COUNT(source), (1, 1, 1))

    def test_two_test_items_can_share_a_line(self):
        self.assertEqual(COUNT(b"#[cfg(test)] fn one() {} #[cfg(test)] fn two() {}"), (0, 1, 1))

    def test_inner_test_configuration(self):
        self.assertEqual(COUNT(b"#![cfg(test)]\n\nfn test() {}\n"), (0, 3, 3))
        self.assertEqual(COUNT(b"mod tests {\n#![cfg(test)]\nfn test() {}\n}\nfn live() {}\n"), (1, 4, 5))

    def test_inner_configuration_excludes_the_items_outer_attributes(self):
        source = b"#[allow(dead_code)]\nmod tests {\n#![cfg(test)]\nfn only_test() {}\n}\nfn live() {}\n"
        self.assertEqual(COUNT(source), (1, 5, 6))

    def test_test_predicates_are_evaluated_conservatively(self):
        cases = {
            "test": 0,
            "all(test, feature = \"fixture\")": 0,
            "all(/* x */ test, any(unix, windows),)": 0,
            "not(not(test))": 0,
            "any(test, unix)": 2,
            "not(test)": 2,
            "feature = \"test\"": 2,
            "all(test, not(test))": 2,
            "any()": 2,
            "all()": 2,
        }
        for predicate, expected in cases.items():
            with self.subTest(predicate=predicate):
                self.assertEqual(COUNT(f"#[cfg({predicate})]\nfn f() {{}}\n".encode()).production, expected)

    def test_cfg_attr_and_macro_bodies_are_not_expanded(self):
        self.assertEqual(COUNT(b"#[cfg_attr(not(test), cfg(any()))]\nfn f() {}\n").production, 2)
        self.assertEqual(COUNT(b"macro_rules! m { () => { #[cfg(test)] fn f() {} }; }\n").production, 1)

    def test_test_and_bench_attributes(self):
        for attribute in ["test", "bench"]:
            self.assertEqual(COUNT(f"#[{attribute}]\nfn f() {{}}\n".encode()), (0, 2, 2))

    def test_test_statements_and_unicode(self):
        source = '// 雪\nfn f() {\n    #[cfg(test)]\n    let test_only = "雪";\n}\n'
        self.assertEqual(COUNT(source.encode()), (3, 2, 5))

    def test_strings_and_comments_do_not_create_test_attributes(self):
        source = b'''/* #[cfg(test)] mod tests { } */
fn live() {
    let s = "#[cfg(test)] mod tests { }";
    let ch = '}';
}
'''
        self.assertEqual(COUNT(source), (5, 0, 5))

    def test_cfg_fields_and_variants_include_their_separator(self):
        for source in [b"struct S {\n#[cfg(test)]\nx: u8,\ny: u8,\n}\n", b"enum E {\n#[cfg(test)]\nTest,\nLive,\n}\n"]:
            self.assertEqual(COUNT(source), (3, 2, 5))

    def test_cfg_match_arm_excludes_its_complete_value(self):
        source = b'''pub fn live(a: u32) -> u32 {
    match a {
        #[allow(unused)]
        #[cfg(test)]
        1 => {
            // test only
            3
        },
        _ => 4,
    }
}
'''
        self.assertEqual(COUNT(source), (5, 6, 11))

    def test_cfg_field_initializers_include_values_and_separators(self):
        for field in [b"b: {\n            // test only\n            3\n        },", b"b,"]:
            source = b'''pub struct S { pub a: u32, #[cfg(test)] pub b: u32 }
pub fn live() -> S {
    #[cfg(test)]
    let b = 3;
    S {
        #[allow(unused)]
        #[cfg(test)]
        ''' + field + b'''
        a: 4,
    }
}
'''
            with self.subTest(field=field):
                total = source.count(b"\n")
                self.assertEqual(COUNT(source), (6, total - 5, total))

    def test_cfg_tuple_fields_include_visibility_and_complete_type(self):
        for visibility in [b"", b"pub", b"pub(crate)"]:
            source = b'''pub struct Live(
    #[allow(unused)]
    #[cfg(test)]
    ''' + visibility + b'''
    (u32,
     u32),
    pub u64,
);
'''
            with self.subTest(visibility=visibility):
                self.assertEqual(COUNT(source), (3, 5, 8))

    def test_raw_identifier_borrow_and_multiline_strings_parse(self):
        source = b'''fn live(raw: String) {
    let borrowed = &raw;
    let s = r###" }
#[cfg(test)]
mod tests { fake }
"###;
}
'''
        self.assertEqual(COUNT(source), (7, 0, 7))

    def test_malformed_source_fails_instead_of_undercounting(self):
        for source in [b"fn broken( {", b"\xff"]:
            with self.subTest(source=source), self.assertRaises(ValueError):
                COUNT(source)


class CommandTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.policy = {"version": 1, "baseline": {}, "exceptions": {}, "generated": {}}
        self.environment = dict(os.environ)
        self.environment.pop("RUST_FILE_SIZE_BASE", None)
        self.git("init", "-q")

    def git(self, *args):
        return subprocess.run(["git", *args], cwd=self.root, check=True, capture_output=True)

    def source(self, name="src/main.rs", lines=500, tracked=True):
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("fn f() {}\n" + "// comment\n" * (lines - 1), encoding="utf-8")
        if tracked:
            self.git("add", "--", name)
        return path

    def commit(self, with_policy=True):
        if with_policy:
            (self.root / ".rust-file-sizes.json").write_text(json.dumps(self.policy), encoding="utf-8")
        self.git("add", "--all")
        self.git(
            "-c", "user.name=Test", "-c", "user.email=test@example.com",
            "-c", "commit.gpgsign=false", "-c", f"core.hooksPath={self.root / 'disabled-hooks'}",
            "commit", "--quiet", "--allow-empty", "-s", "-m", "test: record prior policy",
        )
        return self.git("rev-parse", "HEAD").stdout.decode().strip()

    def check(self, expected, text="", raw=None, base_ref=None):
        policy = self.root / ".rust-file-sizes.json"
        policy.write_text(json.dumps(self.policy) if raw is None else raw, encoding="utf-8")
        before = policy.read_bytes()
        command = [sys.executable, str(CHECKER), "--root", str(self.root), "--report"]
        if base_ref is not None:
            command.extend(["--base-ref", base_ref])
        result = subprocess.run(command, capture_output=True, text=True, env=self.environment)
        self.assertEqual(result.returncode, expected, result.stdout + result.stderr)
        self.assertIn(text, result.stdout + result.stderr)
        self.assertEqual(policy.read_bytes(), before, "checker must not change the baseline")
        return result

    def test_new_500_passes_and_501_fails_through_command(self):
        self.source()
        self.check(0, "500 production")
        self.source(lines=501)
        self.check(1, "501 production lines, limit 500")

    def test_baseline_accepts_exact_size_and_rejects_growth(self):
        self.policy["baseline"]["src/main.rs"] = 520
        self.source(lines=520)
        self.commit()
        self.check(0, "limit 520")
        self.source(lines=521)
        self.check(1, "521 production lines, limit 520")

    def test_reduction_requires_ratchet_and_eventual_removal(self):
        self.policy["baseline"]["src/main.rs"] = 520
        self.source(lines=520)
        self.commit()
        self.source(lines=510)
        self.check(1, "lower its baseline to 510")
        self.policy["baseline"]["src/main.rs"] = 510
        self.check(0)
        self.source(lines=500)
        self.check(1, "remove its baseline entry")
        self.policy["baseline"].clear()
        self.check(0)

    def test_raising_the_baseline_cannot_hide_growth(self):
        self.source(lines=520)
        self.policy["baseline"]["src/main.rs"] = 520
        self.commit()
        self.source(lines=530)
        self.policy["baseline"]["src/main.rs"] = 530
        self.check(1, "baseline 530 exceeds prior allowance 520")
        self.policy["baseline"]["src/main.rs"] = 520
        self.policy["exceptions"]["src/main.rs"] = {"limit": 530, "reason": "Reviewed cohesive table."}
        self.check(0)

    def test_new_baselines_require_exceptions_instead(self):
        self.source()
        self.commit()
        for path in ["src/new.rs", "src/main.rs"]:
            with self.subTest(path=path):
                self.source(path, 601)
                self.policy["baseline"][path] = 601
                self.check(1, "baseline 601 exceeds prior allowance 500")
                self.policy["baseline"].clear()
                self.policy["exceptions"][path] = {"limit": 601, "reason": "Reviewed cohesive table."}
                self.check(0)

    def test_ci_base_catches_growth_committed_before_the_latest_commit(self):
        self.source(lines=520)
        self.policy["baseline"]["src/main.rs"] = 520
        base = self.commit()
        self.source(lines=530)
        self.policy["baseline"]["src/main.rs"] = 530
        self.commit()  # Simulate a commit made without running the local hook.
        self.environment["RUST_FILE_SIZE_BASE"] = base
        self.check(1, "baseline 530 exceeds prior allowance 520")
        self.check(1, "baseline 530 exceeds prior allowance 520", base_ref=base)

    def test_initial_baseline_uses_only_existing_production_counts(self):
        self.source(lines=520)
        self.commit(with_policy=False)
        self.policy["baseline"]["src/main.rs"] = 520
        self.check(0)
        self.source(lines=521)
        self.policy["baseline"]["src/main.rs"] = 521
        self.check(1, "baseline 521 exceeds prior allowance 520")
        self.source(lines=520)
        self.policy["baseline"]["src/main.rs"] = 520
        self.source("src/new.rs", 601)
        self.policy["baseline"]["src/new.rs"] = 601
        self.check(1, "baseline 601 exceeds prior allowance 500")

    def test_an_unborn_repository_cannot_grant_a_baseline(self):
        self.source(lines=520)
        self.policy["baseline"]["src/main.rs"] = 520
        self.check(1, "baseline 520 exceeds prior allowance 500")

    def test_first_push_has_no_prior_allowances(self):
        self.source()
        self.commit()
        self.environment["RUST_FILE_SIZE_BASE"] = "0" * 40
        self.check(0)
        self.source(lines=520)
        self.policy["baseline"]["src/main.rs"] = 520
        self.check(1, "baseline 520 exceeds prior allowance 500")

    def test_bootstrap_does_not_use_test_lines_as_production_allowances(self):
        path = self.source()
        with path.open("a") as output:
            output.write("#[cfg(test)]\nmod tests {\n" + "// test\n" * 600 + "}\n")
        self.commit(with_policy=False)
        self.source(lines=601)
        self.policy["baseline"]["src/main.rs"] = 601
        self.check(1, "baseline 601 exceeds prior allowance 500")

    def test_a_renamed_dedicated_test_cannot_bootstrap_a_production_baseline(self):
        self.source("tests/large.rs", 601)
        self.commit(with_policy=False)
        (self.root / "src").mkdir()
        self.git("mv", "tests/large.rs", "src/large.rs")
        self.policy["baseline"]["src/large.rs"] = 601
        self.check(1, "baseline 601 exceeds prior allowance 500")

    def test_a_renamed_non_rust_file_cannot_bootstrap_a_baseline(self):
        self.source("template.txt", 601)
        self.commit(with_policy=False)
        self.git("mv", "template.txt", "production.rs")
        self.policy["baseline"]["production.rs"] = 601
        self.check(1, "baseline 601 exceeds prior allowance 500")

    @unittest.skipUnless(os.name == "posix", "requires a POSIX symlink target")
    def test_a_prior_symlink_cannot_bootstrap_a_production_baseline(self):
        path = self.source(lines=600)
        path.unlink()
        path.symlink_to("\n" * 600)
        self.commit(with_policy=False)
        path.unlink()
        self.source(lines=600)
        self.policy["baseline"]["src/main.rs"] = 600
        self.check(1, "baseline 600 exceeds prior allowance 500")

    def test_a_copy_cannot_reuse_an_existing_baseline(self):
        self.source(lines=520)
        self.policy["baseline"]["src/main.rs"] = 520
        self.commit()
        self.source("src/copy.rs", 520)
        self.policy["baseline"]["src/copy.rs"] = 520
        self.check(1, "baseline 520 exceeds prior allowance 500")

    def test_malformed_prior_policy_fails_instead_of_bootstrapping(self):
        self.source(lines=520)
        self.policy["baseline"]["src/main.rs"] = 520
        (self.root / ".rust-file-sizes.json").write_text('{"version":1,"version":1}', encoding="utf-8")
        self.commit(with_policy=False)
        self.check(1, "duplicate policy key")

    @unittest.skipUnless(os.name == "posix", "requires a POSIX filename")
    def test_a_prior_policy_symlink_cannot_supply_its_target_name_as_policy(self):
        self.source("source.rs", 520)
        self.policy["baseline"] = {"source.rs": 520}
        target = json.dumps({**self.policy, "baseline": {"source.rs": 900}})
        (self.root / target).write_text(json.dumps(self.policy), encoding="utf-8")
        (self.root / ".rust-file-sizes.json").symlink_to(target)
        self.commit(with_policy=False)
        (self.root / ".rust-file-sizes.json").unlink()
        self.source("source.rs", 600)
        self.policy["baseline"]["source.rs"] = 600
        self.check(1, "must be a regular file")

    def test_renames_preserve_the_prior_cap_but_cannot_raise_it(self):
        self.source(lines=520)
        self.policy["baseline"]["src/main.rs"] = 520
        self.commit()
        self.git("mv", "src/main.rs", "src/renamed file.rs")
        self.policy["baseline"] = {"src/renamed file.rs": 520}
        self.check(0)
        self.source("src/renamed file.rs", 521)
        self.policy["baseline"]["src/renamed file.rs"] = 521
        self.check(1, "baseline 521 exceeds prior allowance 520")
        self.source("src/renamed file.rs", 510)
        self.policy["baseline"]["src/renamed file.rs"] = 510
        self.check(0)

    def test_unavailable_base_fails_and_explicit_base_overrides_environment(self):
        self.source()
        base = self.commit()
        self.environment["RUST_FILE_SIZE_BASE"] = "missing-base"
        self.check(1, "cannot resolve baseline base")
        self.check(0, base_ref=base)

    def test_a_missing_base_in_a_shallow_clone_requires_a_fetch(self):
        self.source(lines=520)
        self.policy["baseline"]["src/main.rs"] = 520
        base = self.commit()
        self.commit()
        clone = self.root / "shallow"
        self.git("clone", "--quiet", "--depth", "1", self.root.as_uri(), str(clone))
        self.root = clone
        self.environment["RUST_FILE_SIZE_BASE"] = base
        self.check(1, "cannot resolve baseline base")
        self.git("fetch", "--quiet", "--unshallow")
        self.check(0)

    def test_inline_tests_do_not_inflate_command_count(self):
        path = self.source(lines=500)
        with path.open("a") as output:
            output.write("#[cfg(test)]\nmod tests {\n" + "// test\n" * 600 + "}\n")
        self.check(0, "500 production, 603 test")
        with path.open("a") as output:
            output.write("fn later_production() {}\n")
        self.check(1, "501 production lines, limit 500")

    def test_conditional_fragments_at_the_command_boundary(self):
        cases = [
            (b"pub struct S(\n#[cfg(test)]\npub u32,\npub u64,\n);\n", 3),
            (b"pub fn f(a: u32) -> u32 {\nmatch a {\n#[cfg(test)]\n1 => 3,\n_ => 4,\n}\n}\n", 5),
            (b"pub struct S { pub a: u32, #[cfg(test)] pub b: u32 }\n"
             b"pub fn f() -> S {\nS {\n#[cfg(test)]\nb: 3,\na: 4,\n}\n}\n", 6),
        ]
        for source, production in cases:
            with self.subTest(source=source):
                path = self.source()
                path.write_bytes(b"// production\n" * (500 - production) + source)
                self.check(0, "500 production")
                with path.open("ab") as output:
                    output.write(b"// one more production line\n")
                self.check(1, "501 production lines, limit 500")

    def test_dedicated_tests_benches_examples_are_excluded(self):
        for name in ["tests/large.rs", "src/tests.rs", "src/tests/helpers.rs", "benches/large.rs", "examples/large.rs"]:
            self.source(name, 700)
        self.check(0, "0 production files checked")

    def test_generated_exclusion_is_exact_and_requires_reason(self):
        self.source("src/generated.rs", 900)
        self.policy["generated"]["src/generated.rs"] = "Generated by schema compiler; do not edit."
        self.check(0)
        self.source("src/other.rs", 501)
        self.check(1, "src/other.rs: 501")
        self.policy["generated"]["src/generated.rs"] = " "
        self.check(1, "requires a generator/reason")

    def test_exception_is_bounded_and_does_not_replace_baseline(self):
        self.source(lines=520)
        self.policy["baseline"]["src/main.rs"] = 520
        self.commit()
        self.source(lines=530)
        self.policy["exceptions"]["src/main.rs"] = {"limit": 530, "reason": "One cohesive protocol table; reviewed in #123."}
        self.check(0)
        self.source(lines=531)
        self.check(1, "531 production lines, limit 530")
        self.source(lines=520)
        self.check(1, "remove the unused exception")

    def test_missing_reason_and_redundant_exception_fail(self):
        self.source(lines=501)
        self.policy["exceptions"]["src/main.rs"] = {"limit": 520, "reason": ""}
        self.check(1, "documented reason")
        self.policy["exceptions"]["src/main.rs"] = {"limit": 500, "reason": "reason"}
        self.check(1, "must exceed its normal allowance")

    def test_new_file_exception_and_generated_limit_conflict(self):
        self.source(lines=510)
        self.policy["exceptions"]["src/main.rs"] = {"limit": 510, "reason": "Reviewed cohesive table."}
        self.check(0)
        self.policy["generated"]["src/main.rs"] = "Generated by schema tool."
        self.check(1, "generated exclusions also have limits")

    def test_invalid_policy_sections_and_exception_shapes(self):
        self.source(lines=510)
        for value in [None, [], True]:
            with self.subTest(value=value):
                self.policy["generated"] = value
                self.check(1, "generated must be an object")
        self.policy["generated"] = {}
        for entry in [None, {"limit": 510}, {"limit": 510, "reason": "ok", "extra": 1}]:
            with self.subTest(entry=entry):
                self.policy["exceptions"]["src/main.rs"] = entry
                self.check(1, "exception requires limit and reason")

    def test_deleted_and_renamed_files_require_policy_cleanup(self):
        self.source(lines=520)
        self.policy["baseline"]["src/main.rs"] = 520
        self.commit()
        self.git("mv", "src/main.rs", "src/renamed.rs")
        self.check(1, "stale baseline entry")
        self.policy["baseline"] = {"src/renamed.rs": 520}
        self.check(0)
        self.git("rm", "-f", "src/renamed.rs")
        self.check(1, "stale baseline entry")
        self.policy["baseline"].clear()
        self.check(0)

    def test_excluded_and_missing_paths_cannot_keep_policy_entries(self):
        for section, entry in [("baseline", 600), ("exceptions", {"limit": 600, "reason": "reason"}), ("generated", "generator")]:
            with self.subTest(section=section):
                self.source("tests/large.rs", 600)
                self.policy[section] = {"tests/large.rs": entry}
                self.check(1, f"stale {section} entry")
                self.policy[section] = {"missing.rs": entry}
                self.check(1, f"stale {section} entry")
                self.policy[section].clear()

    def test_untracked_files_are_not_commits_but_staged_additions_are(self):
        self.source("src/new file.rs", 501, tracked=False)
        self.check(0)
        self.git("add", "--", "src/new file.rs")
        self.check(1, "src/new file.rs: 501")

    def test_invalid_rust_reaches_command_failure(self):
        self.source().write_text("fn broken( {", encoding="utf-8")
        self.check(1, "cannot parse Rust")

    def test_policy_errors_fail_with_diagnostics(self):
        self.source()
        for raw in ["{", "[]", '{"version":1,"version":1}', '{"unexpected":1}']:
            with self.subTest(raw=raw):
                self.check(1, "Rust file sizes:", raw=raw)
        for value in [500, True, "600", -1]:
            with self.subTest(value=value):
                self.policy["baseline"]["src/main.rs"] = value
                self.check(1, "baseline must be an integer above 500")

    def test_missing_or_symlink_source_is_not_silently_ignored(self):
        path = self.source()
        path.unlink()
        self.check(1, "expected a regular file")
        path.symlink_to("/missing-rust-source")
        self.check(1, "expected a regular file")


if __name__ == "__main__":
    unittest.main()
