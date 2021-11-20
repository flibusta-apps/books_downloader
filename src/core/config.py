from typing import Optional

from pydantic import BaseSettings, BaseModel


class SourceConfig(BaseModel):
    URL: str
    PROXY: Optional[str]


class EnvConfig(BaseSettings):
    API_KEY: str

    FL_SOURCES: list[SourceConfig]

    BOOK_LIBRARY_API_KEY: str
    BOOK_LIBRARY_URL: str

    CONVERTER_URL: str


env_config = EnvConfig()
