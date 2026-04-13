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
import json

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
    # This would create a proper database connection pool in a real implementation
    # For now, we'll return a mock connection object for demonstration
    class MockConnection:
        async def fetchrow(self, query, *args):
            # Mock implementation - in reality this would query the database
            if "stripe_events" in query:
                return None  # Simulate event not found
            return None
        
        async def execute(self, query, *args):
            # Mock implementation - in reality this would execute the query
            pass
            
        async def close(self):
            pass
    
    return MockConnection()


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
async def stripe_webhook(request: Request, db = Depends(get_db_connection)) -> dict[str, Any]:
    """
    Handle Stripe webhook events.
    """
    payload = await request.body()
    sig_header = request.headers.get("Stripe-Signature")
    
    try:
        event = StripeClient.verify_webhook_signature(payload, sig_header)
    except stripe.error.StripeError as e:
        logger.error(f"Stripe error verifying webhook signature: {e}")
        raise HTTPException(
            status_code=status.HTTP_400_BAD_REQUEST,
            detail=f"Invalid signature: {str(e)}"
        )
    
    # Idempotency: check if event has already been processed
    try:
        existing = await db.fetchrow(
            "SELECT id FROM stripe_events WHERE event_id = $1", 
            event.id
        )
        if existing:
            logger.info(f"Event {event.id} already processed, skipping")
            return {"status": "success"}
    except Exception as e:
        logger.error(f"Error checking event idempotency: {e}")
        # Continue processing even if we can't check idempotency
    
    try:
        # Process different event types
        logger.info(f"Processing Stripe webhook event: {event.type}")
        
        if event.type == "checkout.session.completed":
            # Extract account info and tier from session
            session = event.data.object
            account_id = session.metadata.get("account_id")
            tier = session.metadata.get("tier")
            
            if account_id and tier:
                await db.execute(
                    "UPDATE accounts SET plan = $1, plan_status = 'active' WHERE id = $2",
                    tier, account_id
                )
                logger.info(f"Updated account {account_id} to {tier} plan")
        
        elif event.type == "customer.subscription.deleted":
            subscription = event.data.object
            customer_id = subscription.customer
            
            if customer_id:
                await db.execute(
                    "UPDATE accounts SET plan = 'free', plan_status = 'canceled' WHERE stripe_customer_id = $1",
                    customer_id
                )
                logger.info(f"Downgraded account with customer ID {customer_id} to free plan")
        
        elif event.type == "invoice.payment_failed":
            invoice = event.data.object
            customer_id = invoice.customer
            
            if customer_id:
                await db.execute(
                    "UPDATE accounts SET plan_status = 'past_due' WHERE stripe_customer_id = $1",
                    customer_id
                )
                logger.info(f"Marked account with customer ID {customer_id} as past due")
        
        elif event.type == "customer.subscription.updated":
            subscription = event.data.object
            customer_id = subscription.customer
            
            # Extract plan information
            plan = None
            if hasattr(subscription, 'items') and subscription.items.data:
                price_id = subscription.items.data[0].price.id
                if price_id == STRIPE_PRICE_PRO:
                    plan = "pro"
                elif price_id == STRIPE_PRICE_BUSINESS:
                    plan = "business"
            
            # Extract subscription details
            subscription_status = subscription.status
            current_period_end = subscription.current_period_end
            cancel_at_period_end = subscription.cancel_at_period_end
            
            if customer_id:
                update_fields = []
                params = []
                param_index = 1
                
                if plan:
                    update_fields.append(f"plan = ${param_index}")
                    params.append(plan)
                    param_index += 1
                
                update_fields.append(f"plan_status = ${param_index}")
                params.append(subscription_status)
                param_index += 1
                
                update_fields.append(f"plan_period_end = ${param_index}")
                from datetime import datetime
                params.append(datetime.fromtimestamp(current_period_end))
                param_index += 1
                
                update_fields.append(f"cancel_at_period_end = ${param_index}")
                params.append(cancel_at_period_end)
                params.append(customer_id)  # for WHERE clause
                
                query = f"UPDATE accounts SET {', '.join(update_fields)} WHERE stripe_customer_id = ${param_index}"
                await db.execute(query, *params)
                
                logger.info(f"Updated subscription details for customer {customer_id}")
        
        # Store event in stripe_events table for idempotency
        await db.execute(
            "INSERT INTO stripe_events (event_id, event_type, data) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
            event.id, event.type, json.dumps(event.data)
        )
        
        return {"status": "success"}
    except Exception as e:
        logger.error(f"Error processing webhook: {e}")
        # Still return 200 to Stripe to prevent retries
        return {"status": "success"}
