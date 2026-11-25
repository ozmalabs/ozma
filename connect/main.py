import logging

from fastapi import FastAPI
from connect.auth import router as auth_router
from connect.controllers import router as controllers_router
from connect.nodes import router as nodes_router
from connect.relay import router as relay_router
from connect.billing import router as billing_router

logger = logging.getLogger(__name__)

app = FastAPI(title="Ozma Connect API", version="0.1.0")

app.include_router(auth_router)
app.include_router(controllers_router)
app.include_router(nodes_router)
app.include_router(relay_router)
app.include_router(billing_router)


def _include_asset_routes() -> None:
    """Install the shared asset router without making it a hard dependency."""
    from connect.assets.router import router as assets_router

    app.include_router(assets_router)


def _configure_asset_service() -> None:
    """Configure the shared asset database without failing unrelated routes.

    A missing or invalid deployment configuration leaves ``/assets`` returning
    ``503``; it must never take the rest of the Connect API down.  Warnings are
    deliberately sanitized: database URLs and signing keys are never logged.
    """
    warning = (
        "shared asset database is not configured; "
        "/assets routes will return 503"
    )
    try:
        from connect.assets.bootstrap import BootstrapError, configure_assets

        configure_assets(app)
    except ImportError as exc:
        app.state.asset_bootstrap_warning = warning
        logger.warning("%s (%s)", warning, type(exc).__name__)
    except BootstrapError as exc:
        app.state.asset_bootstrap_warning = warning
        app.state.asset_bootstrap_error = exc.code
        logger.warning("%s (%s)", warning, exc.code)
    except Exception as exc:  # pragma: no cover - defensive boundary
        app.state.asset_bootstrap_warning = warning
        logger.warning("%s (%s)", warning, type(exc).__name__)


try:
    _include_asset_routes()
except ImportError:
    # Assets are optional in deployments that do not use the shared database.
    pass
else:
    _configure_asset_service()
