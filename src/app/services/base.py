from typing import Protocol, Optional


class BaseDownloader(Protocol):
    @classmethod
    async def download(
        cls, remote_id: int, file_type: str, source_id: int
    ) -> Optional[tuple[bytes, str]]:
        ...
