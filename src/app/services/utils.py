from concurrent.futures.process import ProcessPoolExecutor
import re
import tempfile
import zipfile

import transliterate

from app.services.book_library import Book, BookAuthor


process_pool_executor = ProcessPoolExecutor(2)


def unzip(temp_zipfile, file_type: str):
    result = tempfile.NamedTemporaryFile(delete=False)

    zip_file = zipfile.ZipFile(temp_zipfile)
    for name in zip_file.namelist():  # type: str
        if file_type.lower() in name.lower() or name.lower() == "elector":
            with zip_file.open(name, "r") as internal_file:
                while chunk := internal_file.read(2048):
                    result.write(chunk)

                result.seek(0)
                return result.name

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

    filename = transliterate.translit(filename, reversed=True)

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
