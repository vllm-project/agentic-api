"""Ensure storage boundaries reject bypasses without mistaking test fixtures for production."""

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from check_storage_boundaries import CORE, STORAGE, violations


class StorageBoundaryTests(unittest.TestCase):
    def check(self, path, source):
        return violations(path, source.encode())

    def test_store_alias_and_inline_path_are_rejected(self):
        for source in ["use crate::storage::ResponseStore as Store;", "fn x() { crate::storage::ConversationStore::disabled(); }"]:
            self.assertTrue(self.check(CORE + "executor/prepare.rs", source))

    def test_typed_handler_and_store_owner_are_allowed(self):
        self.assertFalse(self.check(CORE + "executor/prepare.rs", "use crate::executor::ResponseHandler;"))
        self.assertFalse(self.check(CORE + "executor/modes/response.rs", "use crate::storage::ResponseStore;"))

    def test_sql_and_json_aliases_cannot_bypass_ownership(self):
        self.assertTrue(self.check(CORE + "executor/prepare.rs", "use sqlx::query as execute;"))
        self.assertTrue(self.check(STORAGE + "response.rs", "use serde_json::from_str as decode;"))
        self.assertFalse(self.check(STORAGE + "types/item.rs", "use serde_json::from_str;"))
        self.assertFalse(self.check(CORE + "types/error.rs", "use sqlx::Error;"))
        self.assertTrue(self.check(CORE + "executor/prepare.rs", "use sqlx::{ /* alias */ query as run };"))

    def test_parallel_checkpoint_and_store_operations_are_rejected(self):
        self.assertTrue(self.check(CORE + "executor/session.rs", "struct ResponseCheckpoint {}"))
        for method in ["persist_if_version", "rehydrate_snapshot"]:
            self.assertTrue(self.check(STORAGE + "response.rs", f"fn {method}() {{}}"))

    def test_comments_strings_and_test_fixtures_are_excluded(self):
        source = '// sqlx ResponseStore\nfn live() { let text = "ResponseStore"; }\n#[cfg(test)] mod tests { use sqlx::query; }'
        self.assertFalse(self.check(CORE + "executor/prepare.rs", source))
        self.assertFalse(self.check(CORE + "executor/tests/fixture.rs", "use sqlx::query;"))

    def test_malformed_syntax_fails_closed(self):
        self.assertTrue(self.check(CORE + "executor/prepare.rs", "fn broken("))

    def test_configuration_that_can_run_in_production_is_checked(self):
        self.assertTrue(self.check(CORE + "executor/prepare.rs", '#[cfg(any(test, unix))] fn x() { sqlx::query("SELECT 1"); }'))


if __name__ == "__main__":
    unittest.main()
