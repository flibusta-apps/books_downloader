import asyncio
from typing import Optional, AsyncIterator, cast
import zipfile

import aiofiles
import aiofiles.os
import asynctempfile
import httpx

from app.services.base import BaseDownloader
from app.services.book_library import BookLibraryClient
from app.services.exceptions import NotSuccess, ReceivedHTML, ConvertationError
from app.services.utils import zip, unzip, get_filename, process_pool_executor
from core.config import env_config, SourceConfig


class FLDownloader(BaseDownloader):
    EXCLUDE_UNZIP = ["html"]

    def __init__(self, book_id: int, file_type: str, source_id: int):
        self.book_id = book_id
        self.original_file_type = file_type
        self.source_id = source_id

        self.get_book_data_task = asyncio.create_task(self._get_book_data())
        self.get_content_task = asyncio.create_task(self._get_content())

    @property
    def file_type(self):
        return self.original_file_type.replace("zip", "")

    @property
    def need_zip(self):
        return "zip" in self.original_file_type

    async def get_filename(self) -> str:
        if not self.get_book_data_task.done():
            await asyncio.wait_for(self.get_book_data_task, None)

        book = self.get_book_data_task.result()
        if book is None:
            raise ValueError("Book is None!")

        return get_filename(self.book_id, book, self.file_type)

    async def get_final_filename(self, force_zip: bool = False) -> str:
        if self.need_zip or force_zip:
            return (await self.get_filename()) + ".zip"

        return await self.get_filename()

    async def _download_from_source(
        self, source_config: SourceConfig, file_type: Optional[str] = None
    ) -> tuple[httpx.AsyncClient, httpx.Response, bool]:
        basic_url: str = source_config.URL
        proxy: Optional[str] = source_config.PROXY

        file_type_ = file_type or self.file_type

        if self.file_type in ("fb2", "epub", "mobi"):
            url = basic_url + f"/b/{self.book_id}/{file_type_}"
        else:
            url = basic_url + f"/b/{self.book_id}/download"

        client_kwargs = {"timeout": 10 * 60, "follow_redirects": True}

        if proxy is not None:
            client = httpx.AsyncClient(proxies=httpx.Proxy(url=proxy), **client_kwargs)
        else:
            client = httpx.AsyncClient(**client_kwargs)

        request = client.build_request(
            "GET",
            url,
        )
        try:
            response = await client.send(request, stream=True)
        except asyncio.CancelledError:
            await client.aclose()
            raise

        try:
            content_type = response.headers.get("Content-Type")

            if response.status_code != 200:
                raise NotSuccess(f"Status code is {response.status_code}!")

            if "text/html" in content_type:
                raise ReceivedHTML()

            return client, response, "application/zip" in content_type
        except (asyncio.CancelledError, NotSuccess, ReceivedHTML):
            await response.aclose()
            await client.aclose()
            raise

    @classmethod
    async def _close_other_done(
        cls,
        done_tasks: set[asyncio.Task[tuple[httpx.AsyncClient, httpx.Response, bool]]],
    ):
        for task in done_tasks:
            try:
                data = task.result()

                await data[0].aclose()
                await data[1].aclose()
            except (
                NotSuccess,
                ReceivedHTML,
                ConvertationError,
                FileNotFoundError,
                ValueError,
            ):
                continue

    async def _wait_until_some_done(
        self, tasks: set[asyncio.Task[tuple[httpx.AsyncClient, httpx.Response, bool]]]
    ) -> Optional[tuple[httpx.AsyncClient, httpx.Response, bool]]:
        tasks_ = tasks

        while tasks_:
            done, pending = await asyncio.wait(
                tasks_, return_when=asyncio.FIRST_COMPLETED
            )

            for task in done:
                try:
                    data = task.result()

                    await self._close_other_done(
                        {ttask for ttask in pending if not ttask.cancel()}
                    )

                    await self._close_other_done(
                        {ttask for ttask in done if ttask != task}
                    )

                    return data
                except (
                    NotSuccess,
                    ReceivedHTML,
                    ConvertationError,
                    FileNotFoundError,
                    ValueError,
                ):
                    continue

            tasks_ = pending

        return None

    async def _write_response_content_to_ntf(self, temp_file, response: httpx.Response):
        async for chunk in response.aiter_bytes(2048):
            await temp_file.write(chunk)

        await temp_file.flush()
        await temp_file.seek(0)

    async def _unzip(self, response: httpx.Response) -> Optional[str]:
        async with asynctempfile.NamedTemporaryFile(delete=True) as temp_file:
            await self._write_response_content_to_ntf(temp_file, response)

            await temp_file.flush()

            try:
                return await asyncio.get_event_loop().run_in_executor(
                    process_pool_executor, unzip, temp_file.name, "fb2"
                )
            except (FileNotFoundError, zipfile.BadZipFile):
                return None

    async def _download_with_converting(
        self,
    ) -> tuple[httpx.AsyncClient, httpx.Response, bool]:
        tasks = set()

        for source in env_config.FL_SOURCES:
            tasks.add(
                asyncio.create_task(self._download_from_source(source, file_type="fb2"))
            )

        data = await self._wait_until_some_done(tasks)

        if data is None:
            raise ValueError

        client, response, is_zip = data

        try:
            if is_zip:
                filename_to_convert = await self._unzip(response)
            else:
                async with asynctempfile.NamedTemporaryFile(delete=False) as temp_file:
                    await self._write_response_content_to_ntf(temp_file, response)
                    filename_to_convert = temp_file.name
        finally:
            await response.aclose()
            await client.aclose()

        if filename_to_convert is None:
            raise ValueError

        form = {"format": self.file_type}
        files = {"file": open(filename_to_convert, "rb")}

        converter_client = httpx.AsyncClient(timeout=5 * 60)
        converter_request = converter_client.build_request(
            "POST", env_config.CONVERTER_URL, data=form, files=files
        )

        try:
            converter_response = await converter_client.send(
                converter_request, stream=True
            )
        except httpx.ReadTimeout:
            await converter_client.aclose()
            raise ConvertationError()
        except asyncio.CancelledError:
            await converter_client.aclose()
            raise
        finally:
            await aiofiles.os.remove(filename_to_convert)

        try:
            if response.status_code != 200:
                raise ConvertationError

            return converter_client, converter_response, False
        except (asyncio.CancelledError, ConvertationError):
            await converter_response.aclose()
            await converter_client.aclose()
            await aiofiles.os.remove(filename_to_convert)
            raise

    async def _get_content(self) -> Optional[tuple[AsyncIterator[bytes], str]]:
        tasks = set()

        for source in env_config.FL_SOURCES:
            tasks.add(asyncio.create_task(self._download_from_source(source)))

        if self.file_type.lower() in ["epub", "mobi"]:
            tasks.add(asyncio.create_task(self._download_with_converting()))

        data = await self._wait_until_some_done(tasks)

        if data is None:
            return None

        client, response, is_zip = data

        try:
            if is_zip and self.file_type.lower() not in self.EXCLUDE_UNZIP:
                temp_filename = await self._unzip(response)
            else:
                async with asynctempfile.NamedTemporaryFile(delete=False) as temp_file:
                    temp_filename = temp_file.name
                    await self._write_response_content_to_ntf(temp_file, response)
        finally:
            await response.aclose()
            await client.aclose()

        if temp_filename is None:
            return None

        if self.need_zip:
            content_filename = await asyncio.get_event_loop().run_in_executor(
                process_pool_executor, zip, await self.get_filename(), temp_filename
            )
            await aiofiles.os.remove(temp_filename)
        else:
            content_filename = temp_filename

        force_zip = is_zip and self.file_type.lower() in self.EXCLUDE_UNZIP

        async def _content_iterator() -> AsyncIterator[bytes]:
            try:
                async with aiofiles.open(content_filename, "rb") as temp_file:
                    while chunk := await temp_file.read(2048):
                        yield cast(bytes, chunk)
            finally:
                await aiofiles.os.remove(content_filename)

        return _content_iterator(), await self.get_final_filename(force_zip)

    async def _get_book_data(self):
        return await BookLibraryClient.get_remote_book(self.source_id, self.book_id)

    async def _download(self) -> Optional[tuple[AsyncIterator[bytes], str]]:
        await asyncio.wait([self.get_book_data_task, self.get_content_task])

        return self.get_content_task.result()

    @classmethod
    async def download(
        cls, remote_id: int, file_type: str, source_id: int
    ) -> Optional[tuple[AsyncIterator[bytes], str]]:
        downloader = cls(remote_id, file_type, source_id)
        return await downloader._download()
