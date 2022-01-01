FROM python:3.10-slim as build-image

# RUN apt-get update \
#     && apt-get install --no-install-recommends -y gcc build-essential python3-dev libpq-dev libffi-dev \
#     && rm -rf /var/lib/apt/lists/*

WORKDIR /root/poetry
COPY pyproject.toml poetry.lock /root/poetry/

RUN pip install poetry --no-cache-dir \
    && poetry export --without-hashes > requirements.txt

ENV VENV_PATH=/opt/venv
RUN python -m venv $VENV_PATH \
    && . "${VENV_PATH}/bin/activate" \
    && pip install -r requirements.txt --no-cache-dir


FROM python:3.10-slim as runtime-image

# RUN apt-get update \
#     && apt-get install --no-install-recommends -y wget python3-dev libpq-dev libffi-dev default-mysql-client-core \
#     && rm -rf /var/lib/apt/lists/*

COPY ./src/ /app/

ENV VENV_PATH=/opt/venv
COPY --from=build-image $VENV_PATH $VENV_PATH
ENV PATH="$VENV_PATH/bin:$PATH"

EXPOSE 8080

WORKDIR /app/

CMD uvicorn main:app --host="0.0.0.0" --port="8080"
