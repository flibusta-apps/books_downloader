FROM python:3.10-slim as build-image

WORKDIR /root/poetry
COPY pyproject.toml poetry.lock /root/poetry/

RUN pip install poetry wheel --no-cache-dir \
    && poetry export --without-hashes > requirements.txt

ENV VENV_PATH=/opt/venv
RUN python -m venv $VENV_PATH \
    && . "${VENV_PATH}/bin/activate" \
    && pip install -r requirements.txt --no-cache-dir


FROM python:3.10-slim as runtime-image

COPY ./src/ /app/

ENV VENV_PATH=/opt/venv
COPY --from=build-image $VENV_PATH $VENV_PATH
ENV PATH="$VENV_PATH/bin:$PATH"

COPY ./scripts/healthcheck.py /root/

EXPOSE 8080

WORKDIR /app/

CMD gunicorn -k uvicorn.workers.UvicornWorker main:app --bind 0.0.0.0:8080
