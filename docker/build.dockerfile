FROM ghcr.io/flibusta-apps/base_docker_images:3.11-poetry-buildtime as build-image

WORKDIR /root/poetry
COPY pyproject.toml poetry.lock /root/poetry/

ENV VENV_PATH=/opt/venv

RUN poetry export --without-hashes > requirements.txt \
    && . /opt/venv/bin/activate \
    && pip install -r requirements.txt --no-cache-dir


FROM python:3.11-slim as runtime-image

ENV VENV_PATH=/opt/venv
ENV PATH="$VENV_PATH/bin:$PATH"

COPY ./src/ /app/
COPY ./scripts/* /root/
COPY --from=build-image $VENV_PATH $VENV_PATH

EXPOSE 8080

WORKDIR /app/

CMD bash /root/start.sh
