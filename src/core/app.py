from fastapi import FastAPI

from app.views import router


def start_app() -> FastAPI:
    app = FastAPI()

    app.include_router(router)

    return app
