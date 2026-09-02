.PHONY: test debs oci

test:
	./tools/test-local.sh

debs:
	./packaging/build-debs.sh all

oci:
	./oci/build-local.sh
