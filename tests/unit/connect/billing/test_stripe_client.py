import pytest
from unittest.mock import patch, MagicMock
import stripe
from connect.billing.stripe_client import StripeClient
import os

# Set test environment variables
os.environ["STRIPE_SECRET_KEY"] = "sk_test_123"
os.environ["STRIPE_WEBHOOK_SECRET"] = "whsec_test_123"


class TestStripeClient:
    @patch("connect.billing.stripe_client.stripe.Customer.list")
    @patch("connect.billing.stripe_client.stripe.Customer.create")
    def test_get_or_create_customer_existing(self, mock_create, mock_list):
        # Mock existing customer
        mock_customer = MagicMock()
        mock_customer.id = "cus_123"
        mock_list.return_value.data = [mock_customer]
        
        # Call the method
        result = StripeClient.get_or_create_customer("account_123", "test@example.com")
        
        # Assertions
        assert result == mock_customer
        mock_list.assert_called_once_with(metadata={"account_id": "account_123"})
        mock_create.assert_not_called()
    
    @patch("connect.billing.stripe_client.stripe.Customer.list")
    @patch("connect.billing.stripe_client.stripe.Customer.create")
    def test_get_or_create_customer_new(self, mock_create, mock_list):
        # Mock no existing customer
        mock_list.return_value.data = []
        mock_customer = MagicMock()
        mock_customer.id = "cus_new"
        mock_create.return_value = mock_customer
        
        # Call the method
        result = StripeClient.get_or_create_customer("account_123", "test@example.com")
        
        # Assertions
        assert result == mock_customer
        mock_list.assert_called_once_with(metadata={"account_id": "account_123"})
        mock_create.assert_called_once_with(
            email="test@example.com",
            metadata={"account_id": "account_123"}
        )
    
    @patch("connect.billing.stripe_client.stripe.Webhook.construct_event")
    def test_verify_webhook_signature_valid(self, mock_construct_event):
        # Mock a valid event
        mock_event = MagicMock()
        mock_event.type = "checkout.session.completed"
        mock_construct_event.return_value = mock_event
        
        # Test data
        payload = b'{"id": "evt_123"}'
        sig_header = "t=123,v1=hmac"
        
        # Call the method
        result = StripeClient.verify_webhook_signature(payload, sig_header)
        
        # Assertions
        assert result == mock_event
        mock_construct_event.assert_called_once_with(
            payload, sig_header, "whsec_test_123"
        )
    
    @patch("connect.billing.stripe_client.stripe.Webhook.construct_event")
    def test_verify_webhook_signature_invalid(self, mock_construct_event):
        # Mock signature verification failure
        mock_construct_event.side_effect = stripe.error.SignatureVerificationError(
            "Invalid signature", "sig_header"
        )
        
        # Test data
        payload = b'{"id": "evt_123"}'
        sig_header = "t=123,v1=invalid"
        
        # Call the method and expect exception
        with pytest.raises(stripe.error.SignatureVerificationError):
            StripeClient.verify_webhook_signature(payload, sig_header)
