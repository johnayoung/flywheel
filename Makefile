VERSION ?= $(shell git describe --tags --always --dirty 2>/dev/null || echo dev)
LDFLAGS := -ldflags "-s -w -X main.version=$(VERSION)"

.PHONY: build test fmt vet clean

build:
	@mkdir -p bin
	go build $(LDFLAGS) -o bin/flywheel ./cmd/flywheel

test:
	go test -race ./...

cover:
	go test -race -coverprofile=coverage.out ./...
	go tool cover -html=coverage.out -o coverage.html

vet:
	go vet ./...

fmt:
	gofmt -w .

clean:
	rm -rf bin
	rm -f coverage.out coverage.html
