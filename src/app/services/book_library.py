from typing import Generic, TypeVar
import json

import httpx

from datetime import date
from pydantic import BaseModel

from core.config import env_config


T = TypeVar('T')


class Page(BaseModel, Generic[T]):
    items: list[T]
    total: int
    page: int
    size: int


class Source(BaseModel):
    id: int
    name: str


class BookAuthor(BaseModel):
    id: int
    first_name: str
    last_name: str
    middle_name: str


class Book(BaseModel):
    id: int
    title: str
    lang: str
    file_type: str
    uploaded: date
    authors: list[BookAuthor]


class BookLibraryClient:
    API_KEY = env_config.BOOK_LIBRARY_API_KEY
    BASE_URL = env_config.BOOK_LIBRARY_URL

    @classmethod
    @property
    def auth_headers(cls):
        return {'Authorization': cls.API_KEY}

    @classmethod
    async def _make_request(cls, url) -> dict:
        async with httpx.AsyncClient() as client:
            response = await client.get(url, headers=cls.auth_headers)
            return response.json()

    @classmethod
    async def get_sources(cls) -> list[Source]:
        data = await cls._make_request(f"{cls.BASE_URL}/api/v1/sources")

        page = Page[Source].parse_obj(data)

        return [Source.parse_obj(item) for item in page.items]

    @classmethod
    async def get_remote_book(cls, source_id: int, book_id: int) -> Book:
        data = await cls._make_request(f"{cls.BASE_URL}/api/v1/books/remote/{source_id}/{book_id}")

        return Book.parse_obj(data)
