CARGO ?= cargo

.PHONY: all install

all:
	$(CARGO) build --release --locked

install:
	$(CARGO) install --path . --locked --force
