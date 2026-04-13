from fastapi import APIRouter, Request, Depends, HTTPException, status
import stripe
from typing import Any
import logging
import os
from connect.billing.models import (
    BillingStatus,
    CheckoutRequest,
    CheckoutResponse,
    PortalResponse
)
from connect.billing.stripe_client import StripeClient

logger = logging.getLogger(__name__)
router = APIRouter(prefix="/billing", tags=["billing"])

# Get webhook secret from environment
STRIPE_WEBHOOK_SECRET = os.getenv("STRIPE_WEBHOOK_SECRET")


@router.post("/checkout")
async def create_checkout_session(request: CheckoutRequest) -> CheckoutResponse:
    """
    Create a Stripe checkout session.
    """
    # TODO: Implement checkout session creation
    raise HTTPException(
        status_code=status.HTTP_501_NOT_IMPLEMENTED,
        detail="Not implemented"
    )


@router.get("/portal")
async def get_customer_portal() -> PortalResponse:
    """
    Get the Stripe customer portal URL.
    """
    # TODO: Implement customer portal URL generation
    raise HTTPException(
        status_code=status.HTTP_501_NOT_IMPLEMENTED,
        detail="Not implemented"
    )


@router.get("/status")
async def get_billing_status() -> BillingStatus:
    """
    Get the current billing status for the authenticated account.
    """
    # TODO: Implement billing status retrieval
    raise HTTPException(
        status_code=status.HTTP_501_NOT_IMPLEMENTED,
        detail="Not implemented"
    )


@router.post("/webhook")
async def stripe_webhook(request: Request) -> dict[str, Any]:
    """
    Handle Stripe webhook events.
    """
    payload = await request.body()
    sig_header = request.headers.get("Stripe-Signature")
    
    try:
        event = StripeClient.verify_webhook_signature(payload, sig_header)
        
        # TODO: Process different event types
        logger.info(f"Received Stripe webhook event: {event.type}")
        
        # TODO: Store event in stripe_events table for idempotency
        
        return {"status": "success"}
    except Exception as e:
        logger.error(f"Error processing webhook: {e}")
        raise HTTPException(
            status_code=status.HTTP_400_BAD_REQUEST,
            detail="Webhook processing failed"
        )
