.PHONY: web community plugins edge demo demo-community demo-commercial demo-fleet gateway-image check-web check-relay check-gateway-relay check-editions

# Which plan the stand-in entitlement service hands out: hosted-free or pro.
PLAN ?= hosted-free
# The demo stacks are meant to be started and thrown away, so they carry a
# password; `make community` still refuses to boot without one.
DEMO_ADMIN_PASSWORD ?= demo-admin-password

web:
	cd web && python3 -m http.server 8081

community:
	docker compose up --build


plugins:
	docker compose --profile plugins up --build

edge:
	docker compose --profile edge up --build

demo:
	docker compose --profile plugins --profile edge up --build

# The free side: no entitlement service, so the API answers Community itself.
demo-community:
	ADMIN_PASSWORD=$(DEMO_ADMIN_PASSWORD) docker compose up --build

# The paid side: the same core, plus a stand-in for the commercial control
# plane. `make demo-commercial PLAN=pro` for the unlimited plan.
demo-commercial:
	ADMIN_PASSWORD=$(DEMO_ADMIN_PASSWORD) \
	DEMO_PLAN=$(PLAN) \
	ENTITLEMENTS_URL=http://entitlements:8088 \
	docker compose --profile commercial up --build

# Five cameras into whichever stack is up, then what the API kept.
demo-fleet:
	ADMIN_PASSWORD=$(DEMO_ADMIN_PASSWORD) ./scripts/demo-fleet.sh

gateway-image:
	docker build -f edge/gateway/Dockerfile -t vms-gateway:latest .

check-web:
	node --check web/theme.js
	node --check web/dashboard.js
	python3 -m json.tool web/brand.json >/dev/null
	python3 -m json.tool plugins.d/ai-http.json >/dev/null
	python3 -m json.tool plugins.d/storage-s3.json >/dev/null

check-relay:
	./scripts/check-relay.sh

check-gateway-relay:
	./scripts/check-gateway-relay.sh

check-editions:
	./scripts/check-editions.sh
