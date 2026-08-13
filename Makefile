.PHONY: test portable-app oci

test:
	./tools/test-local.sh

portable-app:
	./host/build-portable-app.sh

oci:
	./oci/build-local.sh
