from typing import Protocol


class BaseDownloader(Protocol):
    @classmethod
    async def download(
        cls, remote_id: int, file_type: str, source_id: int
    ) -> tuple[bytes, str]:
        ...
