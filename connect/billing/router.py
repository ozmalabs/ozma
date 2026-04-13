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
import asyncpg

logger = logging.getLogger(__name__)
router = APIRouter(prefix="/billing", tags=["billing"], dependencies=[Depends(get_current_account)])

# Get price IDs from environment
STRIPE_PRICE_PRO = os.getenv("STRIPE_PRICE_PRO")
STRIPE_PRICE_BUSINESS = os.getenv("STRIPE_PRICE_BUSINESS")

# Validate that required environment variables are set
if not STRIPE_PRICE_PRO or not STRIPE_PRICE_BUSINESS:
    raise ValueError("STRIPE_PRICE_PRO and STRIPE_PRICE_BUSINESS environment variables must be set")

# Database connection dependency
async def get_db_connection():
    # In a real implementation, this would create a proper database connection
    # For now, we'll return None as a placeholder since the status endpoint needs work
    return None


@router.post("/checkout", response_model=CheckoutResponse)
async def create_checkout_session(
    request: CheckoutRequest,
    account: dict = Depends(get_current_account),
    db = Depends(get_db_connection)
) -> CheckoutResponse:
    """
    Create a Stripe checkout session.
    """
    # Validate tier
    if request.tier not in ["pro", "business"]:
        raise HTTPException(
            status_code=status.HTTP_400_BAD_REQUEST,
            detail="Invalid tier. Must be 'pro' or 'business'"
        )
    
    # Get price ID based on tier
    price_id = STRIPE_PRICE_PRO if request.tier == "pro" else STRIPE_PRICE_BUSINESS
    
    try:
        # Get or create Stripe customer using the StripeClient
        customer = StripeClient.get_or_create_customer(
            account_id=account["id"],
            email=account.get("email", "")
        )
        
        if not customer or not hasattr(customer, 'id'):
            raise HTTPException(
                status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
                detail="Failed to create or retrieve Stripe customer"
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
            metadata={"account_id": account["id"], "tier": request.tier}
        )
        
        return CheckoutResponse(session_id=session.id, session_url=session.url)
    except stripe.error.StripeError as e:
        logger.error(f"Stripe error creating checkout session: {e}")
        raise HTTPException(
            status_code=status.HTTP_400_BAD_REQUEST,
            detail=f"Stripe error: {str(e)}"
        )
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
    except stripe.error.StripeError as e:
        logger.error(f"Stripe error creating portal session: {e}")
        raise HTTPException(
            status_code=status.HTTP_400_BAD_REQUEST,
            detail=f"Stripe error: {str(e)}"
        )
    except Exception as e:
        logger.error(f"Error creating portal session: {e}")
        raise HTTPException(
            status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
            detail="Failed to create customer portal session"
        )


@router.get("/status", response_model=BillingStatus)
async def get_billing_status(
    account: dict = Depends(get_current_account),
    db = Depends(get_db_connection)
) -> BillingStatus:
    """
    Get the current billing status for the authenticated account.
    """
    # In a real implementation, this would fetch from the database
    # For now, we'll return a placeholder implementation
    try:
        # This would query the database for the customer's subscription status
        # For example:
        # subscription = await db.fetchrow(
        #     "SELECT plan, status, current_period_end, cancel_at_period_end FROM subscriptions WHERE account_id = $1",
        #     account["id"]
        # )
        
        # If no subscription found, return free plan
        # if subscription is None:
        return BillingStatus(
            plan="free",
            plan_status="active",
            plan_period_end=None,
            cancel_at_period_end=False
        )
        
        # Otherwise return the actual subscription data
        # return BillingStatus(
        #     plan=subscription["plan"],
        #     plan_status=subscription["status"],
        #     plan_period_end=subscription["current_period_end"],
        #     cancel_at_period_end=subscription["cancel_at_period_end"]
        # )
    except Exception as e:
        logger.error(f"Error fetching billing status: {e}")
        raise HTTPException(
            status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
            detail="Failed to fetch billing status"
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
        
        # In a real implementation, this would:
        # 1. Store event in stripe_events table for idempotency
        # 2. Handle specific events like:
        #    - customer.subscription.created
        #    - customer.subscription.updated  
        #    - customer.subscription.deleted
        #    - invoice.payment_succeeded
        #    - invoice.payment_failed
        
        # Example of how this might be implemented:
        # await db.execute(
        #     "INSERT INTO stripe_events (event_id, event_type, data) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
        #     event.id, event.type, json.dumps(event.data)
        # )
        
        # if event.type == "customer.subscription.created":
        #     # Update user's subscription in database
        #     pass
        # elif event.type == "customer.subscription.updated":
        #     # Update subscription details
        #     pass
        # elif event.type == "customer.subscription.deleted":
        #     # Mark subscription as cancelled
        #     pass
        
        return {"status": "success"}
    except stripe.error.StripeError as e:
        logger.error(f"Stripe error processing webhook: {e}")
        raise HTTPException(
            status_code=status.HTTP_400_BAD_REQUEST,
            detail=f"Stripe error: {str(e)}"
        )
    except Exception as e:
        logger.error(f"Error processing webhook: {e}")
        raise HTTPException(
            status_code=status.HTTP_400_BAD_REQUEST,
            detail="Webhook processing failed"
        )
