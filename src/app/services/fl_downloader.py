import asyncio
from typing import Optional, cast

import httpx

from app.services.base import BaseDownloader
from app.services.book_library import BookLibraryClient, Book
from app.services.utils import zip, unzip, get_filename, process_pool_executor
from core.config import env_config, SourceConfig


class NotSuccess(Exception):
    pass


class ReceivedHTML(Exception):
    pass


class ConvertationError(Exception):
    pass


class FLDownloader(BaseDownloader):
    def __init__(self, book_id: int, file_type: str, source_id: int):
        self.book_id = book_id
        self.original_file_type = file_type
        self.source_id = source_id

        self.book: Optional[Book] = None

    @property
    def file_type(self):
        return self.original_file_type.replace("zip", "")

    @property
    def need_zip(self):
        return "zip" in self.original_file_type

    async def get_filename(self) -> str:
        if not self.get_book_data_task.done():
            await asyncio.wait_for(self.get_book_data_task, None)

        if self.book is None:
            raise ValueError("Book is None!")

        return get_filename(self.book_id, self.book, self.file_type)

    async def get_final_filename(self) -> str:
        if self.need_zip:
            return (await self.get_filename()) + ".zip"

        return await self.get_filename()

    async def _download_from_source(
        self, source_config: SourceConfig, file_type: str = None
    ) -> tuple[bytes, bool]:
        basic_url: str = source_config.URL
        proxy: Optional[str] = source_config.PROXY

        file_type_ = file_type or self.file_type

        if self.file_type in ("fb2", "epub", "mobi"):
            url = basic_url + f"/b/{self.book_id}/{file_type_}"
        else:
            url = basic_url + f"/b/{self.book_id}/download"

        httpx_proxy = None
        if proxy is not None:
            httpx_proxy = httpx.Proxy(url=proxy)

        async with httpx.AsyncClient(proxies=httpx_proxy) as client:
            response = await client.get(url, follow_redirects=True, timeout=10 * 60)
            content_type = response.headers.get("Content-Type")

            if response.status_code != 200:
                raise NotSuccess(f"Status code is {response.status_code}!")

            if "text/html" in content_type:
                raise ReceivedHTML()

            if "application/zip" in content_type:
                return response.content, True

            return response.content, False

    async def _wait_until_some_done(
        self, tasks: set[asyncio.Task]
    ) -> Optional[tuple[bytes, bool]]:
        tasks_ = tasks

        while tasks_:
            done, pending = await asyncio.wait(
                tasks_, return_when=asyncio.FIRST_COMPLETED
            )

            for task in done:
                try:
                    data = cast(tuple[bytes, bool], task.result())

                    for p_task in pending:
                        p_task.cancel()

                    return data
                except (NotSuccess, ReceivedHTML, ConvertationError):
                    continue

            tasks_ = pending

        return None

    async def _download_with_converting(self) -> tuple[bytes, bool]:
        tasks = set()

        for source in env_config.FL_SOURCES:
            tasks.add(
                asyncio.create_task(self._download_from_source(source, file_type="fb2"))
            )

        data = await self._wait_until_some_done(tasks)

        if data is None:
            raise ValueError

        content, is_zip = data

        if is_zip:
            content = await asyncio.get_event_loop().run_in_executor(
                process_pool_executor, unzip, content, "fb2"
            )

        async with httpx.AsyncClient() as client:
            form = {"format": self.file_type}
            files = {"file": content}
            response = await client.post(
                env_config.CONVERTER_URL, data=form, files=files, timeout=2 * 60
            )

            if response.status_code != 200:
                raise ConvertationError

        return content, False

    async def _get_book_data(self):
        self.book = await BookLibraryClient.get_remote_book(
            self.source_id, self.book_id
        )

    async def _get_content(self) -> tuple[bytes, str]:
        tasks = set()

        if self.file_type in ["epub", "mobi"]:
            tasks.add(asyncio.create_task(self._download_with_converting()))

        for source in env_config.FL_SOURCES:
            tasks.add(asyncio.create_task(self._download_from_source(source)))

        data = await self._wait_until_some_done(tasks)

        if data is None:
            raise ValueError

        content, is_zip = data

        if content is None or is_zip is None:
            raise ValueError

        if is_zip:
            content = await asyncio.get_event_loop().run_in_executor(
                process_pool_executor, unzip, content, self.file_type
            )

        if self.need_zip:
            content = await asyncio.get_event_loop().run_in_executor(
                process_pool_executor, zip, await self.get_filename(), content
            )

        return content, await self.get_final_filename()

    async def _download(self):
        self.get_book_data_task = asyncio.create_task(self._get_book_data())

        tasks = [
            asyncio.create_task(self._get_content()),
            self.get_book_data_task,
        ]

        await asyncio.wait(tasks)

        return tasks[0].result()

    @classmethod
    async def download(
        cls, remote_id: int, file_type: str, source_id: int
    ) -> tuple[bytes, str]:
        downloader = cls(remote_id, file_type, source_id)
        return await downloader._download()
