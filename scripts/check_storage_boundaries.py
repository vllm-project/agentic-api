"""Check storage ownership without compiling Rust (also run by pre-commit)."""

import subprocess
import sys
from pathlib import Path

from check_rust_file_sizes import PARSER, dedicated_test, test_ranges


CORE = "crates/agentic-server-core/src/"
STORAGE = CORE + "storage/"
STORE_OWNERS = {
    CORE + "executor/modes/response.rs", CORE + "executor/modes/conversation.rs",
    CORE + "lib.rs", CORE + "executor/request.rs",  # Public exports and dependency construction.
}
# These providers own extension-specific SQL, as documented in ARCHITECTURE.md.
INDEX_SQL = {CORE + "tool/file_search/sqlite_vec/mod.rs", CORE + "tool/file_search/sqlite_vec/search.rs"}
JSON_READERS = {"serde_json", "serialize_to_string"}
QUERY_APIS = {b"query", b"query_as", b"query_scalar", b"raw_sql", b"QueryBuilder"}


def identifiers(node):
    if node.type in {"identifier", "type_identifier"}:
        return {node.text}
    return {name for child in node.named_children for name in identifiers(child)}


def violations(path, source):
    """Return diagnostics for executable syntax, excluding complete test-only items."""
    parts = Path(path).parts
    if (dedicated_test(path) or any(part.endswith("_tests") for part in parts)
            or Path(path).stem.endswith("_tests") or "/src/" not in path):
        return []
    tree = PARSER.parse(source)
    if tree.root_node.has_error:
        return ["cannot parse Rust; fix syntax or update the pinned grammar"]
    tests = list(test_ranges(tree.root_node))
    errors = set()

    def visit(node):
        if any(start <= node.start_byte and node.end_byte <= end for start, end in tests):
            return
        if node.type in {"line_comment", "block_comment", "string_literal", "raw_string_literal"}:
            return
        row = node.start_point.row + 1
        if node.type in {"identifier", "type_identifier"}:
            text = node.text.decode()
            if text in {"ResponseStore", "ConversationStore"} and not (path.startswith(STORAGE) or path in STORE_OWNERS):
                errors.add(f"{row}: use executor handlers; response/conversation stores belong to executor/modes")
            if text in JSON_READERS and path.startswith(STORAGE) and (
                path in {STORAGE + "response.rs", STORAGE + "conversation.rs"} or path.startswith(STORAGE + "models/")
            ):
                errors.add(f"{row}: stored JSON conversions belong in storage/types")
        if node.type in {"use_declaration", "scoped_identifier", "scoped_type_identifier"} and not (
                path.startswith(STORAGE) or path in INDEX_SQL):
            names = identifiers(node)
            if b"sqlx" in names and names & QUERY_APIS:
                errors.add(f"{row}: SQL belongs in storage; call the owning typed API")
        if node.type == "struct_item":
            name = node.child_by_field_name("name").text.decode()
            if name == "ResponseCheckpoint" and path != STORAGE + "types/response.rs":
                errors.add(f"{row}: extend storage/types::ResponseCheckpoint instead of defining another checkpoint")
        if node.type == "function_item" and path in {STORAGE + "response.rs", STORAGE + "conversation.rs"}:
            name = node.child_by_field_name("name").text.decode()
            if (name.startswith("persist") and name != "persist") or (name.startswith("rehydrate") and name != "rehydrate"):
                errors.add(f"{row}: extend the existing persist/rehydrate operation instead of adding {name}")
        for child in node.named_children:
            visit(child)

    visit(tree.root_node)
    return sorted(errors)


def main():
    root = Path(__file__).resolve().parents[1]
    paths = subprocess.check_output(["git", "ls-files", "--cached", "--others", "--exclude-standard", "-z", "*.rs"], cwd=root)
    failures = []
    for path in sorted(set(paths.decode().split("\0")) - {""}):
        file = root / path
        if file.is_file():
            failures.extend(f"{path}:{message}" for message in violations(path, file.read_bytes()))
    for failure in failures:
        print(failure, file=sys.stderr)
    return bool(failures)


if __name__ == "__main__":
    sys.exit(main())
