from fastapi import FastAPI
from connect.auth import router as auth_router
from connect.controllers import router as controllers_router
from connect.nodes import router as nodes_router
from connect.relay import router as relay_router
from connect.billing import router as billing_router

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


try:
    _include_asset_routes()
except ImportError:
    # Assets are optional in deployments that do not use the shared database.
    pass
