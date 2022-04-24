from fastapi import FastAPI

from prometheus_fastapi_instrumentator import Instrumentator
import sentry_sdk

from app.views import router, healthcheck_router
from core.config import env_config


sentry_sdk.init(
    env_config.SENTRY_DSN,
)


def start_app() -> FastAPI:
    app = FastAPI()

    app.include_router(router)
    app.include_router(healthcheck_router)

    Instrumentator().instrument(app).expose(app, include_in_schema=True)

    return app
