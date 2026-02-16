# rye:signed:2026-02-16T05:32:16Z:a80f399358997054c2d96936ee823cdd8610ff36b06718f2e0cacd75a7584327:PY-cA7pGwn5etrbL8IgOSNaAy1ssfP9gWGlOirx01_zRqwHcvX6tLEK43hY06YOI1tc_C1Ylf0Vcp5nT2GSBDg==:440443d0858f0199
__version__ = "1.0.0"
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

    async def create_completion(self, messages: List[Dict], tools: List[Dict]) -> Dict:
        """Send messages to LLM and return structured response.

        Args:
            messages: List of {"role": str, "content": str} message dicts
            tools: List of tool schemas the LLM can call

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

    async def create_streaming_completion(
        self, messages: List[Dict], tools: List[Dict]
    ):
        """Streaming variant — yields chunks."""
        raise NotImplementedError(
            f"No streaming provider implementation for model '{self.model}'. "
            "Subclass ProviderAdapter and implement create_streaming_completion()."
        )
