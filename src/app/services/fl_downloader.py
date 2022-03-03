import asyncio
import os
import tempfile
from typing import IO, Optional, AsyncIterator, cast

from fastapi import UploadFile

import httpx

from app.services.base import BaseDownloader
from app.services.book_library import BookLibraryClient
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

    async def get_final_filename(self) -> str:
        if self.need_zip:
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
                await response.aclose()
                await client.aclose()
                raise NotSuccess(f"Status code is {response.status_code}!")

            if "text/html" in content_type:
                await response.aclose()
                await client.aclose()
                raise ReceivedHTML()

            return client, response, "application/zip" in content_type
        except asyncio.CancelledError:
            await client.aclose()
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
            except (NotSuccess, ReceivedHTML, ConvertationError):
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

                    for p_task in pending:
                        p_task.cancel()

                    await self._close_other_done(
                        {ttask for ttask in done if ttask != task}
                    )

                    return data
                except (NotSuccess, ReceivedHTML, ConvertationError):
                    continue

            tasks_ = pending

        return None

    async def _write_response_content_to_ntf(self, ntf, response: httpx.Response):
        temp_file = UploadFile(await self.get_filename(), ntf)

        async for chunk in response.aiter_bytes(2048):
            await temp_file.write(chunk)

        temp_file.file.flush()

        await temp_file.seek(0)

        return temp_file.file

    async def _unzip(self, response: httpx.Response):
        with tempfile.NamedTemporaryFile() as ntf:
            await self._write_response_content_to_ntf(ntf, response)

            internal_tempfile_name = await asyncio.get_event_loop().run_in_executor(
                process_pool_executor, unzip, ntf.name, "fb2"
            )

        return internal_tempfile_name

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

        is_temp_file = False
        try:
            if is_zip:
                file_to_convert_name = await self._unzip(response)
            else:
                file_to_convert = tempfile.NamedTemporaryFile()
                await self._write_response_content_to_ntf(file_to_convert, response)
                file_to_convert_name = file_to_convert.name
                is_temp_file = True
        finally:
            await response.aclose()
            await client.aclose()

        form = {"format": self.file_type}
        files = {"file": open(file_to_convert_name, "rb")}

        converter_client = httpx.AsyncClient(timeout=2 * 60)
        converter_request = converter_client.build_request(
            "POST", env_config.CONVERTER_URL, data=form, files=files
        )
        try:
            converter_response = await converter_client.send(
                converter_request, stream=True
            )
        except asyncio.CancelledError:
            await converter_client.aclose()
            raise
        finally:
            if is_temp_file:
                await asyncio.get_event_loop().run_in_executor(
                    process_pool_executor, os.remove, file_to_convert_name
                )

        if response.status_code != 200:
            raise ConvertationError

        try:
            return converter_client, converter_response, False
        except asyncio.CancelledError:
            await converter_response.aclose()
            await converter_client.aclose()
            raise

    async def _get_content(self) -> Optional[tuple[AsyncIterator[bytes], str]]:
        tasks = set()

        for source in env_config.FL_SOURCES:
            tasks.add(asyncio.create_task(self._download_from_source(source)))

        if self.file_type in ["epub", "mobi"]:
            tasks.add(asyncio.create_task(self._download_with_converting()))

        data = await self._wait_until_some_done(tasks)

        if data is None:
            return None

        client, response, is_zip = data

        try:
            if is_zip:
                temp_file_name = await self._unzip(response)
            else:

                temp_file = tempfile.NamedTemporaryFile()
                await self._write_response_content_to_ntf(temp_file, response)
                temp_file_name = temp_file.name
        finally:
            await response.aclose()
            await client.aclose()

        is_unziped_temp_file = False
        if self.need_zip:
            content_filename = await asyncio.get_event_loop().run_in_executor(
                process_pool_executor, zip, await self.get_filename(), temp_file_name
            )
            is_unziped_temp_file = True
        else:
            content_filename = temp_file_name

        content = cast(IO, open(content_filename, "rb"))

        async def _content_iterator() -> AsyncIterator[bytes]:
            t_file = UploadFile(await self.get_filename(), content)
            try:
                while chunk := await t_file.read(2048):
                    yield cast(bytes, chunk)
            finally:
                await t_file.close()
                if is_unziped_temp_file:
                    await asyncio.get_event_loop().run_in_executor(
                        process_pool_executor, os.remove, content_filename
                    )

        return _content_iterator(), await self.get_final_filename()

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
