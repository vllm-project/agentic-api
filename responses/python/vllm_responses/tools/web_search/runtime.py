from __future__ import annotations

from dataclasses import dataclass
from typing import TYPE_CHECKING

from vllm_responses.configs.runtime import RuntimeConfig
from vllm_responses.mcp.runtime_client import BuiltinMcpRuntimeClient
from vllm_responses.tools.base.runtime import bind_runtime_requirements
from vllm_responses.tools.base.types import BuiltinActionAdapter
from vllm_responses.tools.ids import WEB_SEARCH_TOOL
from vllm_responses.tools.profile_resolution import resolve_profiled_builtin_tool
from vllm_responses.tools.web_search.adapters import WEB_SEARCH_ADAPTER_SPECS
from vllm_responses.tools.web_search.config import resolve_request_config
from vllm_responses.tools.web_search.executor import WebSearchExecutor
from vllm_responses.tools.web_search.page_cache import WebSearchPageCache
from vllm_responses.utils.exceptions import BadInputError

if TYPE_CHECKING:
    from vllm_responses.types.openai import vLLMResponsesRequest


@dataclass(slots=True)
class WebSearchToolRuntime:
    executor: WebSearchExecutor


def build_web_search_tool_runtime(
    *,
    request: "vLLMResponsesRequest",
    enabled_builtin_tool_names: set[str],
    runtime_config: RuntimeConfig,
    builtin_mcp_runtime_client: BuiltinMcpRuntimeClient | None,
) -> WebSearchToolRuntime | None:
    if WEB_SEARCH_TOOL not in enabled_builtin_tool_names or not request.tools:
        return None
    from vllm_responses.types.openai import OpenAIResponsesWebSearchTool

    web_search_tools = [
        tool for tool in request.tools if isinstance(tool, OpenAIResponsesWebSearchTool)
    ]
    if not web_search_tools:
        return None
    if len(web_search_tools) > 1:
        raise BadInputError("Duplicate `web_search` tools are not allowed.")
    try:
        request_config = resolve_request_config(
            tool=web_search_tools[0],
            runtime_config=runtime_config,
        )
        resolved_tool = resolve_profiled_builtin_tool(
            tool_type=WEB_SEARCH_TOOL,
            profile_id=request_config.profile_id,
        )
        bound_requirements = bind_runtime_requirements(
            resolved_tool=resolved_tool,
            builtin_mcp_runtime_client=builtin_mcp_runtime_client,
        )
        adapter_by_action: dict[str, BuiltinActionAdapter] = {
            binding.action_name: WEB_SEARCH_ADAPTER_SPECS[binding.adapter_id].build_adapter()
            for binding in resolved_tool.action_bindings
        }
    except ValueError as exc:
        raise BadInputError(str(exc)) from exc
    return WebSearchToolRuntime(
        executor=WebSearchExecutor(
            request_config=request_config,
            resolved_tool=resolved_tool,
            bound_requirements=bound_requirements,
            adapter_by_action=adapter_by_action,
            page_cache=WebSearchPageCache(),
        )
    )
