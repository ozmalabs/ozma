#!/bin/bash
# Run the full ecosystem E2E test suite.
#
# Usage:
#   ./run.sh              # build, start, test, stop
#   ./run.sh up           # just start services
#   ./run.sh test         # just run tests (services must be running)
#   ./run.sh down         # just stop services
#   ./run.sh logs         # tail service logs

set -euo pipefail
cd "$(dirname "$0")"

case "${1:-all}" in
  up)
    echo "Starting services..."
    docker compose up -d --build --wait
    echo "All services healthy."
    ;;
  test)
    echo "Running E2E tests..."
    docker compose run --rm test-runner
    ;;
  down)
    echo "Stopping services..."
    docker compose down -v
    ;;
  logs)
    docker compose logs -f --tail=50
    ;;
  all)
    echo "=== Building and starting services ==="
    docker compose up -d --build --wait
    echo
    echo "=== Running E2E tests ==="
    docker compose run --rm test-runner
    rc=$?
    echo
    echo "=== Stopping services ==="
    docker compose down -v
    exit $rc
    ;;
  *)
    echo "Usage: $0 [up|test|down|logs|all]"
    exit 1
    ;;
esac
