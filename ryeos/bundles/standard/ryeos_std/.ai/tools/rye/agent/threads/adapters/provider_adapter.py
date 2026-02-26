# rye:signed:2026-02-26T05:02:40Z:82a912043ead08055ddf38383848f3123be10125934836f92f9d31b5048c79b1:IsZjMe1zvVJQqP1zAOpzrKqujB4cYqzeoglWrZBLMz3UCu72MGDwKi_r7ZwO9lhTgSidvgN2QVo5xdjjMUxqBw==:4b987fd4e40303ac
__version__ = "1.2.0"
__tool_type__ = "python"
__category__ = "rye/agent/threads/adapters"
__tool_description__ = "Base provider adapter interface"

from typing import Any, Dict, List


class ProviderAdapter:
    """Abstract interface for LLM providers.

    Each provider implementation translates to/from the provider's native API.
    The runner calls only these methods.
    """

    def __init__(self, model: str, provider_config: Dict):
        self.model = model
        self.config = provider_config

    @property
    def tool_use_mode(self) -> str:
        """Whether to use native API tool_use or text-parsed tool calls."""
        return self.config.get("tool_use", {}).get("mode", "native")

    async def create_completion(
        self, messages: List[Dict], tools: List[Dict], system_prompt: str = ""
    ) -> Dict:
        """Send messages to LLM and return structured response.

        Args:
            messages: List of {"role": str, "content": str} message dicts
            tools: List of tool schemas the LLM can call
            system_prompt: Optional system-level instructions (identity, behavior, protocol)

        Returns:
            {
                "text": str,
                "tool_calls": [
                    {
                        "id": str,
                        "name": str,
                        "input": Dict,
                    }
                ],
                "input_tokens": int,
                "output_tokens": int,
                "spend": float,
                "finish_reason": str,
            }
        """
        raise NotImplementedError(
            f"No provider implementation for model '{self.model}'. "
            "Subclass ProviderAdapter and implement create_completion()."
        )

    @property
    def supports_streaming(self) -> bool:
        """Whether this provider supports streaming completions."""
        return False

    async def create_streaming_completion(
        self, messages: List[Dict], tools: List[Dict], sinks: Any = None,
        system_prompt: str = "",
    ) -> Dict:
        """Streaming variant with sink fan-out.

        Sinks receive raw SSE events in real-time. Returns the same
        response dict as create_completion() after stream completes.
        """
        raise NotImplementedError(
            f"No streaming provider implementation for model '{self.model}'. "
            "Subclass ProviderAdapter and implement create_streaming_completion()."
        )
