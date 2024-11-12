use std::{collections::HashMap, mem};

use chia::{
    clvm_utils::CurriedProgram,
    protocol::{Bytes32, Coin},
    puzzles::{
        cat::CatArgs,
        offer::{
            NotarizedPayment, Payment, SettlementPaymentsSolution, SETTLEMENT_PAYMENTS_PUZZLE_HASH,
        },
    },
};
use chia_wallet_sdk::{
    Condition, Conditions, HashedPtr, Layer, NftInfo, Offer, OfferBuilder, SettlementLayer,
    SpendContext, StandardLayer,
};
use indexmap::IndexMap;

use crate::{OfferRequest, OfferedCoins, WalletError};

use super::{
    offer_royalties::{
        calculate_asset_prices, calculate_asset_royalties, calculate_royalty_assertions,
        NftRoyaltyInfo,
    },
    CatOfferSpend, NftOfferSpend, OfferSpend, UnsignedOffer, Wallet,
};

impl Wallet {
    pub async fn make_offer(
        &self,
        offered: OfferedCoins,
        requested: OfferRequest,
        hardened: bool,
        reuse: bool,
    ) -> Result<UnsignedOffer, WalletError> {
        let p2_puzzle_hash = self.p2_puzzle_hash(hardened, reuse).await?;

        // Calculate the royalty payments required for requested NFTs.
        let mut requested_nft_royalty_info = Vec::new();

        for (nft_id, info) in &requested.nfts {
            requested_nft_royalty_info.push(NftRoyaltyInfo {
                launcher_id: *nft_id,
                royalty_puzzle_hash: info.royalty_puzzle_hash,
                royalty_ten_thousandths: info.royalty_ten_thousandths,
            });
        }

        let offered_trade_prices =
            calculate_asset_prices(requested.nfts.len(), offered.xch, &offered.cats)?;

        let royalties_we_pay =
            calculate_asset_royalties(&requested_nft_royalty_info, &offered_trade_prices)?;

        // We need to get a list of all of the coin ids being offered for the nonce.
        let mut coin_ids = Vec::new();

        // Select coins for the XCH being offered.
        let total_xch = offered.xch
            + offered.fee
            + royalties_we_pay
                .iter()
                .fold(0, |acc, royalty| acc + royalty.amount);

        let p2_coins = if total_xch > 0 {
            self.select_p2_coins(total_xch as u128).await?
        } else {
            Vec::new()
        };

        for p2_coin in &p2_coins {
            coin_ids.push(p2_coin.coin_id());
        }

        // Select coins for the CATs being offered.
        let mut cats = IndexMap::new();

        for (&asset_id, &amount) in &offered.cats {
            if amount == 0 {
                continue;
            }

            let cat_coins = self.select_cat_coins(asset_id, amount as u128).await?;

            for cat_coin in &cat_coins {
                coin_ids.push(cat_coin.coin.coin_id());
            }

            cats.insert(asset_id, cat_coins);
        }

        // Calculate trade prices for NFTs being offered.
        let requested_trade_prices =
            calculate_asset_prices(offered.nfts.len(), requested.xch, &requested.cats)?;

        // Fetch coin info for the NFTs being offered.
        let mut nfts = Vec::new();

        for nft_id in offered.nfts {
            let Some(nft) = self.db.nft(nft_id).await? else {
                return Err(WalletError::MissingNft(nft_id));
            };

            coin_ids.push(nft.coin.coin_id());

            nfts.push(NftOfferSpend {
                nft,
                trade_prices: requested_trade_prices.clone(),
            });
        }

        // Calculate royalty info for the NFTs being offered.
        let mut offered_nft_royalty_info = Vec::new();

        for spend in &nfts {
            offered_nft_royalty_info.push(NftRoyaltyInfo {
                launcher_id: spend.nft.info.launcher_id,
                royalty_puzzle_hash: spend.nft.info.royalty_puzzle_hash,
                royalty_ten_thousandths: spend.nft.info.royalty_ten_thousandths,
            });
        }

        let royalties_they_pay =
            calculate_asset_royalties(&offered_nft_royalty_info, &requested_trade_prices)?;

        // Calculate the nonce for the offer.
        let nonce = Offer::nonce(coin_ids);

        // Create the offer builder with the nonce.
        let mut builder = OfferBuilder::new(nonce);
        let mut ctx = SpendContext::new();

        let settlement = ctx.settlement_payments_puzzle()?;
        let cat = ctx.cat_puzzle()?;

        // Add requested XCH payments.
        if requested.xch > 0 {
            builder = builder.request(
                &mut ctx,
                &settlement,
                vec![Payment::new(p2_puzzle_hash, requested.xch)],
            )?;
        }

        // Add requested CAT payments.
        for (asset_id, amount) in requested.cats {
            builder = builder.request(
                &mut ctx,
                &CurriedProgram {
                    program: cat,
                    args: CatArgs::new(asset_id, settlement),
                },
                vec![Payment::with_memos(
                    p2_puzzle_hash,
                    amount,
                    vec![p2_puzzle_hash.into()],
                )],
            )?;
        }

        // Add requested NFT payments.
        for (nft_id, info) in requested.nfts {
            let info = NftInfo {
                launcher_id: nft_id,
                metadata: info.metadata,
                metadata_updater_puzzle_hash: info.metadata_updater_puzzle_hash,
                current_owner: None,
                royalty_puzzle_hash: info.royalty_puzzle_hash,
                royalty_ten_thousandths: info.royalty_ten_thousandths,
                p2_puzzle_hash: SETTLEMENT_PAYMENTS_PUZZLE_HASH.into(),
            };

            let layers = info.into_layers(settlement).construct_puzzle(&mut ctx)?;

            builder = builder.request(
                &mut ctx,
                &layers,
                vec![Payment::with_memos(
                    p2_puzzle_hash,
                    1,
                    vec![p2_puzzle_hash.into()],
                )],
            )?;
        }

        // Finish the requested payments and get the list of announcement assertions.
        let (mut assertions, builder) = builder.finish();

        assertions.extend(calculate_royalty_assertions(&royalties_they_pay));

        self.spend_assets(
            &mut ctx,
            OfferSpend {
                p2_coins,
                p2_amount: offered.xch,
                fee: offered.fee,
                royalties: royalties_we_pay,
                cats: cats
                    .into_iter()
                    .map(|(asset_id, coins)| CatOfferSpend {
                        coins,
                        amount: offered.cats[&asset_id],
                    })
                    .collect(),
                nfts,
                assertions,
                change_puzzle_hash: p2_puzzle_hash,
            },
        )
        .await?;

        // Construct the final offer.
        let coin_spends = ctx.take();

        Ok(UnsignedOffer {
            ctx,
            coin_spends,
            builder,
        })
    }

    async fn spend_assets(
        &self,
        ctx: &mut SpendContext,
        spend: OfferSpend,
    ) -> Result<(), WalletError> {
        let mut assertions =
            Conditions::new().extend(spend.assertions.into_iter().map(Condition::from));

        // Calculate primary coins.
        let mut primary_coins = Vec::new();

        if let Some(p2_coin) = spend.p2_coins.first() {
            primary_coins.push(p2_coin.coin_id());
        }

        for CatOfferSpend { coins, .. } in &spend.cats {
            if let Some(cat) = coins.first() {
                primary_coins.push(cat.coin.coin_id());
            }
        }

        for NftOfferSpend { nft, .. } in &spend.nfts {
            primary_coins.push(nft.coin.coin_id());
        }

        // Calculate conditions for each primary coin.
        let mut primary_conditions = HashMap::new();

        if primary_coins.len() == 1 {
            primary_conditions.insert(primary_coins[0], assertions);
        } else {
            for (i, &coin_id) in primary_coins.iter().enumerate() {
                let relation = if i == 0 {
                    *primary_coins.last().expect("empty primary coins")
                } else {
                    primary_coins[i - 1]
                };

                primary_conditions.insert(
                    coin_id,
                    mem::take(&mut assertions).assert_concurrent_spend(relation),
                );
            }
        }

        // Spend the XCH.
        if !spend.p2_coins.is_empty() {
            let mut conditions = primary_conditions
                .remove(&spend.p2_coins[0].coin_id())
                .unwrap_or_default();

            if spend.p2_amount > 0 {
                conditions = conditions.create_coin(
                    SETTLEMENT_PAYMENTS_PUZZLE_HASH.into(),
                    spend.p2_amount,
                    Vec::new(),
                );
            }

            for royalty in &spend.royalties {
                conditions = conditions.create_coin(
                    royalty.settlement_puzzle_hash,
                    royalty.amount,
                    Vec::new(),
                );

                let royalty_coin = Coin::new(
                    spend.p2_coins[0].coin_id(),
                    royalty.settlement_puzzle_hash,
                    royalty.amount,
                );

                let coin_spend = SettlementLayer.construct_coin_spend(
                    ctx,
                    royalty_coin,
                    SettlementPaymentsSolution {
                        notarized_payments: vec![NotarizedPayment {
                            nonce: royalty.nft_id,
                            payments: vec![Payment::with_memos(
                                royalty.p2_puzzle_hash,
                                royalty.amount,
                                vec![royalty.p2_puzzle_hash.into()],
                            )],
                        }],
                    },
                )?;
                ctx.insert(coin_spend);
            }

            let total: u128 = spend.p2_coins.iter().map(|coin| coin.amount as u128).sum();
            let change = total
                - spend.p2_amount as u128
                - spend.fee as u128
                - spend
                    .royalties
                    .iter()
                    .fold(0u128, |acc, royalty| acc + royalty.amount as u128);

            if change > 0 {
                conditions = conditions.create_coin(
                    spend.change_puzzle_hash,
                    change.try_into().expect("change overflow"),
                    Vec::new(),
                );
            }

            if spend.fee > 0 {
                conditions = conditions.reserve_fee(spend.fee);
            }

            self.spend_p2_coins(ctx, spend.p2_coins, conditions).await?;
        }

        // Spend the CATs.
        for CatOfferSpend { coins, amount } in spend.cats {
            let total: u128 = coins.iter().map(|cat| cat.coin.amount as u128).sum();
            let change = (total - amount as u128)
                .try_into()
                .expect("change overflow");

            self.spend_cat_coins(
                ctx,
                coins.into_iter().enumerate().map(|(i, cat)| {
                    if i > 0 {
                        return (cat, Conditions::new());
                    }

                    let mut conditions = primary_conditions
                        .remove(&cat.coin.coin_id())
                        .unwrap_or_default()
                        .create_coin(
                            SETTLEMENT_PAYMENTS_PUZZLE_HASH.into(),
                            amount,
                            vec![Bytes32::from(SETTLEMENT_PAYMENTS_PUZZLE_HASH).into()],
                        );

                    if change > 0 {
                        conditions = conditions.create_coin(
                            spend.change_puzzle_hash,
                            change,
                            vec![spend.change_puzzle_hash.into()],
                        );
                    }

                    (cat, conditions)
                }),
            )
            .await?;
        }

        // Spend the NFTs.
        for NftOfferSpend { nft, trade_prices } in spend.nfts {
            let metadata_ptr = ctx.alloc(&nft.info.metadata)?;
            let nft = nft.with_metadata(HashedPtr::from_ptr(&ctx.allocator, metadata_ptr));

            let synthetic_key = self.db.synthetic_key(nft.info.p2_puzzle_hash).await?;
            let p2 = StandardLayer::new(synthetic_key);

            let conditions = primary_conditions
                .remove(&nft.coin.coin_id())
                .unwrap_or_default();

            let _ = nft.lock_settlement(ctx, &p2, trade_prices, conditions)?;
        }

        Ok(())
    }
}
