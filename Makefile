.PHONY: test appimage oci

test:
	./tools/test-local.sh

appimage:
	./host/build-appimage.sh

oci:
	./oci/build-local.sh
