#! /usr/bin/env sh

cd /app

/env.sh > ./.env

exec /usr/local/bin/books_downloader
