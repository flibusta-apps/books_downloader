import asyncio
from concurrent.futures.process import ProcessPoolExecutor
import os
import re
import tempfile
from typing import Optional
import zipfile

import transliterate
import transliterate.exceptions

from app.services.book_library import Book, BookAuthor


process_pool_executor = ProcessPoolExecutor(2)


def remove_temp_file(filename: str) -> bool:
    try:
        os.remove(filename)
        return True
    except OSError:
        return False


def unzip(temp_zipfile: str, file_type: str) -> Optional[str]:
    zip_file = zipfile.ZipFile(temp_zipfile)

    result = tempfile.NamedTemporaryFile(delete=False)

    for name in zip_file.namelist():
        if file_type.lower() in name.lower() or name.lower() == "elector":
            with zip_file.open(name, "r") as internal_file:
                while chunk := internal_file.read(2048):
                    result.write(chunk)

                result.seek(0)
                return result.name

    result.close()
    remove_temp_file(result.name)

    raise FileNotFoundError


def zip(
    filename: str,
    content_filename: str,
) -> str:
    result = tempfile.NamedTemporaryFile(delete=False)

    zip_file = zipfile.ZipFile(
        file=result,
        mode="w",
        compression=zipfile.ZIP_DEFLATED,
        allowZip64=False,
        compresslevel=9,
    )

    with open(content_filename, "rb") as content:
        with zip_file.open(filename, "w") as internal_file:
            while chunk := content.read(2048):
                internal_file.write(chunk)

    for zfile in zip_file.filelist:
        zfile.create_system = 0

    zip_file.close()
    result.close()

    return result.name


def get_short_name(author: BookAuthor) -> str:
    name_parts = []

    if author.last_name:
        name_parts.append(author.last_name)

    if author.first_name:
        name_parts.append(author.first_name[:1])

    if author.middle_name:
        name_parts.append(author.middle_name[:1])

    return " ".join(name_parts)


def get_filename(book_id: int, book: Book, file_type: str) -> str:
    filename_parts = []

    file_type_ = "fb2.zip" if file_type == "fb2zip" else file_type

    if book.authors:
        filename_parts.append(
            "_".join([get_short_name(a) for a in book.authors]) + "_-_"
        )

    if book.title.startswith(" "):
        filename_parts.append(book.title[1:])
    else:
        filename_parts.append(book.title)

    filename = "".join(filename_parts)

    try:
        filename = transliterate.translit(filename, reversed=True)
    except transliterate.exceptions.LanguageDetectionError:
        pass

    for c in "(),….’!\"?»«':":
        filename = filename.replace(c, "")

    for c, r in (
        ("—", "-"),
        ("/", "_"),
        ("№", "N"),
        (" ", "_"),
        ("–", "-"),
        ("á", "a"),
        (" ", "_"),
        ("'", ""),
    ):
        filename = filename.replace(c, r)

    filename = re.sub(r"[^\x00-\x7f]", r"", filename)

    right_part = f".{book_id}.{file_type_}"

    return filename[: 64 - len(right_part) - 1] + right_part


def async_retry(*exceptions: type[Exception], times: int = 1, delay: float = 1.0):
    """
    :param times: retry count
    :param delay: delay time
    :param default_content: set default content
    :return
    """

    def func_wrapper(f):
        async def wrapper(*args, **kwargs):
            for retry in range(times):
                try:
                    return await f(*args, **kwargs)
                except exceptions as e:
                    if retry + 1 == times:
                        raise e

                await asyncio.sleep(delay)

        return wrapper

    return func_wrapper
