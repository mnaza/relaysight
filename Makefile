.PHONY: web community plugins edge demo demo-community demo-commercial demo-fleet gateway-image check-web check-relay check-gateway-relay check-editions check-installer check-ingest check-plugin check-plugin-mtls check-sso check-sso-keycloak check-ring-full check-dst check-restore check-alert check-alert-chat check-live

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
	node --check web/wall.js
	python3 -m json.tool web/brand.json >/dev/null
	python3 -m json.tool plugins.d/ai-http.json >/dev/null
	python3 -m json.tool plugins.d/storage-s3.json >/dev/null

check-relay:
	./scripts/check-relay.sh

check-gateway-relay:
	./scripts/check-gateway-relay.sh

check-editions:
	./scripts/check-editions.sh

check-installer:
	./scripts/check-installer.sh

check-ingest:
	./scripts/check-ingest.sh

# Sign in through an identity provider, between processes rather than in one.
check-sso:
	./scripts/check-sso.sh

# The same, against Keycloak: a provider this repository did not write.
check-sso-keycloak:
	./scripts/check-sso-keycloak.sh

# Fill a real filesystem under the ring buffer, rather than a byte budget.
check-ring-full:
	./scripts/check-ring-full.sh

# A schedule across a daylight-saving change, on this box's own zone database.
check-dst:
	./scripts/check-dst.sh

# Back up a running control plane, destroy it, restore it, upgrade an old one.
check-restore:
	./scripts/check-restore.sh

# Live video from a fake camera to decoded frames in a real headless Firefox.
check-live:
	./scripts/check-live.sh

# Raise an alert and follow it to a webhook. WEBHOOK_URL=... to post for real.
check-alert:
	./scripts/check-alert.sh

# The same, into a real chat service: Mattermost in a container, read back out
# of the channel. Needs Docker.
check-alert-chat:
	./scripts/check-alert-chat.sh

# Does a plugin speak the protocol? make check-plugin ENDPOINT=http://localhost:9002
check-plugin-mtls:
	./scripts/check-plugin-mtls.sh

check-plugin:
	@test -n "$(ENDPOINT)" || (echo "set ENDPOINT, e.g. make check-plugin ENDPOINT=http://localhost:9002" && false)
	cargo run -q -p vms-plugin-runtime --example conformance -- "$(ENDPOINT)"
