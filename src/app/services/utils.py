import io
import zipfile

from concurrent.futures.process import ProcessPoolExecutor

import transliterate

from app.services.book_library import Book, BookAuthor


process_pool_executor = ProcessPoolExecutor(2)


def unzip(file_bytes: bytes, file_type: str):
    zip_file = zipfile.ZipFile(io.BytesIO(file_bytes))
    for name in zip_file.namelist():  # type: str
        if file_type in name.lower():
            return zip_file.read(name)
    raise FileNotFoundError


def zip(filename, content):
    buffer = io.BytesIO()
    zip_file = zipfile.ZipFile(
        file=buffer,
        mode='w',
        compression=zipfile.ZIP_DEFLATED,
        allowZip64=False,
        compresslevel=9
    )
    zip_file.writestr(filename, content)

    for zfile in zip_file.filelist:
        zfile.create_system = 0

    zip_file.close()

    buffer.seek(0)

    return buffer.read()


def get_short_name(author: BookAuthor) -> str:
    name_parts = []

    if author.last_name:
        name_parts.append(author.last_name)

    if author.first_name:
        name_parts.append(author.first_name[:1])

    if author.middle_name:
        name_parts.append(author.middle_name[:1])

    return " ".join(name_parts)


def get_filename(book: Book, file_type: str) -> str:
    filename_parts = []

    if book.authors:
        filename_parts.append(
            '_'.join([get_short_name(a) for a in book.authors]) + '_-_'
        )

    if book.title.startswith(" "):
        filename_parts.append(
            book.title[1:]
        )
    else:
        filename_parts.append(
            book.title
        )

    filename = "".join(filename_parts)

    if book.lang in ['ru']:
        filename = transliterate.translit(filename, 'ru', reversed=True)

    for c in "(),….’!\"?»«':":
        filename = filename.replace(c, '')

    for c, r in (('—', '-'), ('/', '_'), ('№', 'N'), (' ', '_'), ('–', '-'), ('á', 'a'), (' ', '_')):
        filename = filename.replace(c, r)

    right_part = f'.{book.id}.{file_type}'

    return filename[:64 - len(right_part)] + right_part
