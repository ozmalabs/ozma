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
from connect.auth.dependencies import get_current_account

logger = logging.getLogger(__name__)
router = APIRouter(prefix="/billing", tags=["billing"], dependencies=[Depends(get_current_account)])

# Get price IDs from environment
STRIPE_PRICE_PRO = os.getenv("STRIPE_PRICE_PRO")
STRIPE_PRICE_BUSINESS = os.getenv("STRIPE_PRICE_BUSINESS")


@router.post("/checkout", response_model=CheckoutResponse)
async def create_checkout_session(
    request: CheckoutRequest,
    account: dict = Depends(get_current_account)
) -> CheckoutResponse:
    """
    Create a Stripe checkout session.
    """
    # Validate tier
    if request.price_id not in ["pro", "business"]:
        raise HTTPException(
            status_code=status.HTTP_400_BAD_REQUEST,
            detail="Invalid price_id. Must be 'pro' or 'business'"
        )
    
    # Get price ID based on tier
    price_id = STRIPE_PRICE_PRO if request.price_id == "pro" else STRIPE_PRICE_BUSINESS
    
    if not price_id:
        raise HTTPException(
            status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
            detail=f"Price ID not configured for tier: {request.price_id}"
        )
    
    try:
        # Get or create Stripe customer using the StripeClient
        customer = StripeClient.get_or_create_customer(
            account_id=account["id"],
            email=account.get("email", "")
        )
                
        # Create checkout session
        session = stripe.checkout.Session.create(
            customer=customer.id,
            mode="subscription",
            line_items=[{
                "price": price_id,
                "quantity": 1
            }],
            success_url="https://connect.ozma.dev/dashboard/billing?success=true",
            cancel_url="https://connect.ozma.dev/dashboard/billing?canceled=true",
            metadata={"account_id": account["id"], "tier": request.price_id}
        )
        
        return CheckoutResponse(session_id=session.id, session_url=session.url)
    except Exception as e:
        logger.error(f"Error creating checkout session: {e}")
        raise HTTPException(
            status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
            detail="Failed to create checkout session"
        )


@router.get("/portal", response_model=PortalResponse)
async def get_customer_portal(account: dict = Depends(get_current_account)) -> PortalResponse:
    """
    Get the Stripe customer portal URL.
    """
    customer_id = account.get("stripe_customer_id")
    if not customer_id:
        raise HTTPException(
            status_code=status.HTTP_400_BAD_REQUEST,
            detail="No Stripe customer associated with this account"
        )
    
    try:
        portal_session = stripe.billing_portal.Session.create(
            customer=customer_id,
            return_url="https://connect.ozma.dev/dashboard/billing"
        )
        return PortalResponse(portal_url=portal_session.url)
    except Exception as e:
        logger.error(f"Error creating portal session: {e}")
        raise HTTPException(
            status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
            detail="Failed to create customer portal session"
        )


@router.get("/status", response_model=BillingStatus)
async def get_billing_status(account: dict = Depends(get_current_account)) -> BillingStatus:
    """
    Get the current billing status for the authenticated account.
    """
    # TODO: Fetch from DB based on account
    # This is a placeholder implementation
    return BillingStatus(
        plan="free",
        plan_status="active",
        plan_period_end=None,
        cancel_at_period_end=False
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
        
        # Process different event types
        logger.info(f"Received Stripe webhook event: {event.type}")
        
        # TODO: Store event in stripe_events table for idempotency
        # TODO: Handle specific events like customer.subscription.created/updated/deleted
        
        return {"status": "success"}
    except Exception as e:
        logger.error(f"Error processing webhook: {e}")
        raise HTTPException(
            status_code=status.HTTP_400_BAD_REQUEST,
            detail="Webhook processing failed"
        )
