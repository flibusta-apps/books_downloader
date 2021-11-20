from app.services.base import BaseDownloader
from app.services.fl_downloader import FLDownloader

from app.services.book_library import BookLibraryClient


class DownloadersManager:
    SOURCES_TABLE: dict[int, str] = {}
    DOWNLOADERS_TABLE: dict[str, type[BaseDownloader]] = {
        'flibusta': FLDownloader,
    }

    PREPARED = False

    @classmethod
    async def _prepare(cls):
        sources = await BookLibraryClient.get_sources()

        for source in sources:
            cls.SOURCES_TABLE[source.id] = source.name

    @classmethod
    async def get_downloader(cls, source_id: int):
        if not cls.PREPARED:
            await cls._prepare()

        name = cls.SOURCES_TABLE[source_id]

        return cls.DOWNLOADERS_TABLE[name]
