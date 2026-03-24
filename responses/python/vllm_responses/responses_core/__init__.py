"""Internal implementation core for Responses API parity.

This package is intentionally separate from `vllm_responses.types`:
- `vllm_responses.types` defines wire-contract Pydantic models and should remain importable without
  pulling in orchestration/state machines.
- `vllm_responses.responses_core` owns internal normalization and contract-composition logic.
"""
