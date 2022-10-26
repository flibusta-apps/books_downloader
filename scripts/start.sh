cd /app

gunicorn -k uvicorn.workers.UvicornWorker main:app --bind 0.0.0.0:8080 --timeout 600
