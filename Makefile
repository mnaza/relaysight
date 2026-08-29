.PHONY: web community commercial plugins edge demo gateway-image check-web

web:
	cd web && python3 -m http.server 8081

community:
	docker compose up --build

commercial:
	docker compose -f docker-compose.yml -f docker-compose.commercial.yml up --build

plugins:
	docker compose --profile plugins up --build

edge:
	docker compose --profile edge up --build

demo:
	docker compose --profile plugins --profile edge up --build

gateway-image:
	docker build -f edge/gateway/Dockerfile -t vms-gateway:latest .

check-web:
	node --check web/theme.js
	node --check web/landing.js
	node --check web/dashboard.js
	python3 -m json.tool web/brand.json >/dev/null
	python3 -m json.tool plugins.d/ai-http.json >/dev/null
	python3 -m json.tool plugins.d/storage-s3.json >/dev/null
