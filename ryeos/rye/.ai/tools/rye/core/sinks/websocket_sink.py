# rye:signed:2026-02-23T00:42:51Z:d025f534cf376e6de33335a810553225bdba675e4e6720530bc936ff54daaf34:onovu3eng_LzYTk4_Kl57aAm6yPcE-oXgyDs0CZTIvzQO6xukK9pOkcjAJb4lz2GtliNolU8bdYz6l3bF_20Dw==:9fbfabe975fa5a7f
__tool_type__ = "runtime"
__version__ = "1.0.0"
__executor_id__ = "python"
__category__ = "rye/core/sinks"
__tool_description__ = (
    "WebSocket sink - forward events to WebSocket endpoint with reconnection support"
)

# Dependencies handled by EnvManager
DEPENDENCIES = ["websockets"]

import asyncio
import json
from typing import List, Optional
import websockets


class WebSocketSink:
    """Forward events to WebSocket endpoint with reconnection support."""

    def __init__(
        self,
        url: str,
        reconnect_attempts: int = 3,
        buffer_on_disconnect: bool = True,
        buffer_max_size: int = 1000,
    ):
        self.url = url
        self.reconnect_attempts = reconnect_attempts
        self.buffer_on_disconnect = buffer_on_disconnect
        self.buffer_max_size = buffer_max_size

        self.ws: Optional[websockets.WebSocketClientProtocol] = None
        self.buffer: List[str] = []
        self.connected = False

    async def _connect(self) -> bool:
        """Establish WebSocket connection with retry."""
        for attempt in range(self.reconnect_attempts):
            try:
                self.ws = await websockets.connect(self.url)
                self.connected = True

                # Flush buffer if we have events
                if self.buffer:
                    for event in self.buffer:
                        await self.ws.send(event)
                    self.buffer.clear()

                return True
            except Exception as e:
                if attempt < self.reconnect_attempts - 1:
                    await asyncio.sleep(0.5 * (2**attempt))  # Exponential backoff
                continue

        return False

    async def write(self, event: str) -> None:
        """Write event to WebSocket."""
        # Ensure connection
        if not self.connected or not self.ws:
            if not await self._connect():
                if self.buffer_on_disconnect:
                    if len(self.buffer) < self.buffer_max_size:
                        self.buffer.append(event)
                return

        try:
            await self.ws.send(event)
        except websockets.ConnectionClosed:
            self.connected = False
            if self.buffer_on_disconnect:
                if len(self.buffer) < self.buffer_max_size:
                    self.buffer.append(event)

    async def close(self) -> None:
        """Close WebSocket connection."""
        if self.ws:
            await self.ws.close()
            self.ws = None
