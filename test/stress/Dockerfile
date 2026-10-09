# the echo servers for scripts/stress.sh (built and loaded into kind on the runner)
FROM golang:1.23-alpine AS build
WORKDIR /src
COPY go.mod main.go ./
RUN CGO_ENABLED=0 go build -trimpath -ldflags=-s -o /stress .
FROM scratch
COPY --from=build /stress /stress
ENTRYPOINT ["/stress", "echo"]
