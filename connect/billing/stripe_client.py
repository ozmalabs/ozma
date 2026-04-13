import stripe
import os
from typing import Any
import logging

# Initialize Stripe with secret key from environment variable
stripe.api_key = os.getenv("STRIPE_SECRET_KEY")
STRIPE_WEBHOOK_SECRET = os.getenv("STRIPE_WEBHOOK_SECRET")

logger = logging.getLogger(__name__)


class StripeClient:
    @staticmethod
    def get_or_create_customer(account_id: str, email: str) -> stripe.Customer:
        """
        Get or create a Stripe customer for the given account.
        
        Args:
            account_id: The internal account ID
            email: Customer email address
            
        Returns:
            stripe.Customer: The Stripe customer object
        """
        try:
            # Try to find existing customer by account ID in metadata
            customers = stripe.Customer.list(metadata={"account_id": account_id})
            if customers.data:
                return customers.data[0]
            
            # Create new customer if not found
            customer = stripe.Customer.create(
                email=email,
                metadata={"account_id": account_id}
            )
            return customer
        except Exception as e:
            logger.error(f"Error getting/creating Stripe customer: {e}")
            raise

    @staticmethod
    def verify_webhook_signature(payload: bytes, sig_header: str) -> stripe.Event:
        """
        Verify the webhook signature from Stripe.
        
        Args:
            payload: The raw webhook payload
            sig_header: The Stripe-Signature header
            
        Returns:
            stripe.Event: The verified Stripe event
            
        Raises:
            stripe.error.SignatureVerificationError: If signature verification fails
        """
        try:
            event = stripe.Webhook.construct_event(
                payload, sig_header, STRIPE_WEBHOOK_SECRET
            )
            return event
        except Exception as e:
            logger.error(f"Webhook signature verification failed: {e}")
            raise
