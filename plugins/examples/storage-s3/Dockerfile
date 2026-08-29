FROM python:3.13-alpine
WORKDIR /app
COPY requirements.txt /app/requirements.txt
RUN pip install --no-cache-dir -r /app/requirements.txt
COPY plugin.py /app/plugin.py
EXPOSE 9002
ENTRYPOINT ["python3", "/app/plugin.py"]
