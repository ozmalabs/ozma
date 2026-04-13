from pydantic import BaseModel
from datetime import datetime
from typing import Optional


class BillingStatus(BaseModel):
    plan: str
    plan_status: str
    plan_period_end: Optional[datetime] = None
    cancel_at_period_end: bool


class CheckoutRequest(BaseModel):
    price_id: str
    success_url: str
    cancel_url: str


class CheckoutResponse(BaseModel):
    session_id: str
    session_url: str


class PortalResponse(BaseModel):
    portal_url: str
